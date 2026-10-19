package com.algoartist.gta5native;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.widget.FrameLayout;
import android.widget.TextView;
import java.io.File;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Direct native Android shell. It owns the session handle and Surface
 * lifecycle; the controller view only emits input events.
 */
public final class MainActivity extends Activity {
    private static final String OBB_ROOT =
            "/sdcard/Android/obb/com.algoartist.gta5native.dev";
    private static final int STOP_TIMEOUT_MS = 1500;

    private final ExecutorService startupExecutor = Executors.newSingleThreadExecutor();
    private final Handler statusHandler = new Handler(Looper.getMainLooper());
    private final Object lifecycleLock = new Object();
    private volatile long nativeHandle;
    private volatile boolean destroyed;
    /** The engine ended its own session ("Exit Game", or the end of a command-line benchmark). */
    private boolean engineExited;
    private TouchControllerView controls;
    private DirectSurfaceView surface;
    private TextView statusView;
    private TitleScreen title;
    private final long[] loadingOut = new long[4];
    private boolean firstFrame, world;
    private int selectedStartMode;
    // (uptime ms, presents) samples of the last second, refreshed every 10 ms

    // The web page's progress channel: engine lines, data bytes and the GPU worker's frames.
    private final Runnable loadingPoll = new Runnable() {
        @Override public void run() {
            if (destroyed || title == null) return;
            long handle = nativeHandle;
            if (handle != 0L) {
                String lines = NativeBridge.loading(handle, loadingOut);
                if (lines != null && !lines.isEmpty()) {
                    for (String line : lines.split("\n", -1)) {
                        title.line(line);
                    }
                }
                title.dataTotal(loadingOut[0]);
                if (!firstFrame && loadingOut[1] > 0) { firstFrame = true; title.firstFrame(); }
                if (!world && loadingOut[2] != 0) { world = true; title.world(); }
                NativeBridge.framePresented(handle, loadingOut[1]);
            }
            statusHandler.postDelayed(this, 10L);
        }
    };

    private final Runnable statusPoll = new Runnable() {
        @Override public void run() {
            if (destroyed) return;
            updateStatus();
            statusHandler.postDelayed(this, 500L);
        }
    };

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(0xff101820);

        surface = new DirectSurfaceView(this);
        root.addView(surface, new FrameLayout.LayoutParams(-1, -1));

        statusView = new TextView(this);
        statusView.setTextColor(0xffdce8ed);
        statusView.setTextSize(14.0f);
        statusView.setGravity(Gravity.CENTER);
        statusView.setText("Starting direct native host…");
        FrameLayout.LayoutParams statusParams = new FrameLayout.LayoutParams(-1, -2,
                Gravity.TOP);
        statusParams.topMargin = 8;
        root.addView(statusView, statusParams);

        controls = new TouchControllerView(this, new TouchControllerView.Sink() {
            @Override public void pad(int buttons, int sticks, int triggers) {
                withHandle(handle -> NativeBridge.pad(handle, buttons, sticks, triggers));
            }

            @Override public void releaseAll() {
                withHandle(NativeBridge::releaseAll);
            }
        });
        root.addView(controls, new FrameLayout.LayoutParams(-1, -1));
        // Cover the desktop startup instructions from the first frame.
        selectedStartMode = getIntent().getIntExtra("restartMode", 0);
        title = new TitleScreen(this, mode -> {
            selectedStartMode = mode;
            withHandle(handle -> NativeBridge.startMode(handle, mode));
        });
        root.addView(title, new FrameLayout.LayoutParams(-1, -1));
        title.begin();
        setContentView(root);

        startupExecutor.execute(this::createAndBoot);
        statusHandler.post(statusPoll);
        statusHandler.post(loadingPoll);
    }

    private void createAndBoot() {
        File saveRoot = new File(getFilesDir(), "saves");
        if (!saveRoot.exists() && !saveRoot.mkdirs()) {
            postStartupFailure("cannot create save directory: " + saveRoot);
            return;
        }
        File assetsRoot = new File(OBB_ROOT, "data");
        // Shader assets are expected beside data at OBB_ROOT/shaders. The
        // native renderer resolves that sibling from the supplied asset root.
        long handle = NativeBridge.create(assetsRoot.getAbsolutePath(),
                saveRoot.getAbsolutePath());
        if (handle == 0L) {
            postStartupFailure("native session creation failed");
            return;
        }
        NativeBridge.registerSession(handle);
        int result = NativeBridge.boot(handle);
        if (result != 0) {
            NativeBridge.destroy(handle);
            NativeBridge.discardSession(handle);
            postStartupFailure("native boot failed: " + result);
            return;
        }
        boolean disposeAfterStart;
        synchronized (lifecycleLock) {
            disposeAfterStart = destroyed;
            if (!disposeAfterStart) nativeHandle = handle;
        }
        if (disposeAfterStart) {
            NativeBridge.stop(handle, STOP_TIMEOUT_MS);
            NativeBridge.destroy(handle);
            NativeBridge.discardSession(handle);
            return;
        }
        runOnUiThread(() -> {
            if (destroyed || nativeHandle != handle) return;
            surface.setNativeHandle(handle);
            NativeBridge.focus(handle, true);
            if (selectedStartMode >= 1 && selectedStartMode <= 3)
                NativeBridge.startMode(handle, selectedStartMode);
            updateStatus();
        });
    }

    private void postStartupFailure(String message) {
        if (destroyed) return;
        runOnUiThread(() -> {
            if (!destroyed) {
                statusView.setText("Direct host unavailable\n" + message);
                title.error(message);
            }
        });
    }

    private interface NativeCall {
        void call(long handle);
    }

    private void withHandle(NativeCall call) {
        long handle = nativeHandle;
        if (handle == 0L || destroyed) return;
        call.call(handle);
    }

    /** Forwards real hardware/ADB key events. No on-screen keyboard is used. */
    @Override public boolean dispatchKeyEvent(KeyEvent event) {
        if (title != null && title.onKey(event)) return true;
        int vk = windowsVk(event.getKeyCode());
        int action = event.getAction();
        if (vk < 0 || (action != KeyEvent.ACTION_DOWN && action != KeyEvent.ACTION_UP)) {
            return super.dispatchKeyEvent(event);
        }
        long handle = nativeHandle;
        if (handle == 0L || destroyed) {
            return super.dispatchKeyEvent(event);
        }
        int source = physicalSource(event);
        NativeBridge.key(handle, source, vk, action == KeyEvent.ACTION_DOWN);
        // Windows sends a character message after each printable key-down; the
        // game's text boxes (the cheat box) read those, not the key codes.
        int character = action == KeyEvent.ACTION_DOWN ? event.getUnicodeChar(event.getMetaState()) : 0;
        if (character >= 0x20 && character != 0x7f) {
            for (char unit : Character.toChars(character)) NativeBridge.textUtf16(handle, unit);
        }
        // Consume mapped keys, including Android BACK mapped to Escape, so
        // the Activity cannot leave the native game on a hardware key event.
        return true;
    }

    private static int physicalSource(KeyEvent event) {
        int device = event.getDeviceId() & 0x7fff;
        int keyCode = event.getKeyCode() & 0xff;
        return NativeBridge.PHYSICAL_KEY_SOURCE_BASE | (device << 8) | keyCode;
    }

    private static int windowsVk(int keyCode) {
        if (keyCode >= KeyEvent.KEYCODE_0 && keyCode <= KeyEvent.KEYCODE_9) {
            return 0x30 + keyCode - KeyEvent.KEYCODE_0;
        }
        if (keyCode >= KeyEvent.KEYCODE_A && keyCode <= KeyEvent.KEYCODE_Z) {
            return 0x41 + keyCode - KeyEvent.KEYCODE_A;
        }
        if (keyCode >= KeyEvent.KEYCODE_F1 && keyCode <= KeyEvent.KEYCODE_F12) {
            return 0x70 + keyCode - KeyEvent.KEYCODE_F1;
        }
        if (keyCode >= KeyEvent.KEYCODE_NUMPAD_0 && keyCode <= KeyEvent.KEYCODE_NUMPAD_9) {
            return 0x60 + keyCode - KeyEvent.KEYCODE_NUMPAD_0;
        }
        switch (keyCode) {
            case KeyEvent.KEYCODE_BACK:
            case KeyEvent.KEYCODE_ESCAPE:
                return 0x1b;
            case KeyEvent.KEYCODE_TAB:
                return 0x09;
            case KeyEvent.KEYCODE_ENTER:
            case KeyEvent.KEYCODE_NUMPAD_ENTER:
                return 0x0d;
            case KeyEvent.KEYCODE_DEL:
                return 0x08;
            case KeyEvent.KEYCODE_FORWARD_DEL:
                return 0x2e;
            case KeyEvent.KEYCODE_INSERT:
                return 0x2d;
            case KeyEvent.KEYCODE_MOVE_HOME:
                return 0x24;
            case KeyEvent.KEYCODE_MOVE_END:
                return 0x23;
            case KeyEvent.KEYCODE_PAGE_UP:
                return 0x21;
            case KeyEvent.KEYCODE_PAGE_DOWN:
                return 0x22;
            case KeyEvent.KEYCODE_DPAD_LEFT:
                return 0x25;
            case KeyEvent.KEYCODE_DPAD_UP:
                return 0x26;
            case KeyEvent.KEYCODE_DPAD_RIGHT:
                return 0x27;
            case KeyEvent.KEYCODE_DPAD_DOWN:
                return 0x28;
            case KeyEvent.KEYCODE_DPAD_CENTER:
                return 0x0d;
            case KeyEvent.KEYCODE_HOME:
                return 0x24;
            case KeyEvent.KEYCODE_CLEAR:
                return 0x0c;
            case KeyEvent.KEYCODE_CAPS_LOCK:
                return 0x14;
            case KeyEvent.KEYCODE_SCROLL_LOCK:
                return 0x91;
            case KeyEvent.KEYCODE_NUM_LOCK:
                return 0x90;
            case KeyEvent.KEYCODE_SYSRQ:
                return 0x2c;
            case KeyEvent.KEYCODE_BREAK:
                return 0x13;
            case KeyEvent.KEYCODE_SHIFT_LEFT:
                return 0xa0;
            case KeyEvent.KEYCODE_SHIFT_RIGHT:
                return 0xa1;
            case KeyEvent.KEYCODE_CTRL_LEFT:
                return 0xa2;
            case KeyEvent.KEYCODE_CTRL_RIGHT:
                return 0xa3;
            case KeyEvent.KEYCODE_ALT_LEFT:
                return 0xa4;
            case KeyEvent.KEYCODE_ALT_RIGHT:
                return 0xa5;
            case KeyEvent.KEYCODE_META_LEFT:
                return 0x5b;
            case KeyEvent.KEYCODE_META_RIGHT:
                return 0x5c;
            case KeyEvent.KEYCODE_MENU:
                return 0x5d;
            case KeyEvent.KEYCODE_SPACE:
                return 0x20;
            case KeyEvent.KEYCODE_COMMA:
            case KeyEvent.KEYCODE_NUMPAD_COMMA:
                return 0xbc;
            case KeyEvent.KEYCODE_PERIOD:
                return 0xbe;
            case KeyEvent.KEYCODE_MINUS:
                return 0xbd;
            case KeyEvent.KEYCODE_EQUALS:
            case KeyEvent.KEYCODE_NUMPAD_EQUALS:
                return 0xbb;
            case KeyEvent.KEYCODE_LEFT_BRACKET:
                return 0xdb;
            case KeyEvent.KEYCODE_RIGHT_BRACKET:
                return 0xdd;
            case KeyEvent.KEYCODE_BACKSLASH:
                return 0xdc;
            case KeyEvent.KEYCODE_SEMICOLON:
                return 0xba;
            case KeyEvent.KEYCODE_APOSTROPHE:
                return 0xde;
            case KeyEvent.KEYCODE_SLASH:
                return 0xbf;
            case KeyEvent.KEYCODE_GRAVE:
                return 0xc0;
            case KeyEvent.KEYCODE_AT:
                return 0x40;
            case KeyEvent.KEYCODE_PLUS:
                return 0xbb;
            case KeyEvent.KEYCODE_STAR:
                return 0x6a;
            case KeyEvent.KEYCODE_POUND:
                return 0x23;
            case KeyEvent.KEYCODE_YEN:
                return 0xdc;
            case KeyEvent.KEYCODE_HELP:
                return 0x2f;
            case KeyEvent.KEYCODE_NUMPAD_DIVIDE:
                return 0x6f;
            case KeyEvent.KEYCODE_NUMPAD_MULTIPLY:
                return 0x6a;
            case KeyEvent.KEYCODE_NUMPAD_SUBTRACT:
                return 0x6d;
            case KeyEvent.KEYCODE_NUMPAD_ADD:
                return 0x6b;
            case KeyEvent.KEYCODE_NUMPAD_DOT:
                return 0x6e;
            default:
                return -1;
        }
    }

    private void updateStatus() {
        long handle = nativeHandle;
        if (handle == 0L || statusView == null || destroyed) return;
        int state = NativeBridge.status(handle);
        String error = NativeBridge.error(handle);
        if (state == NativeBridge.SESSION_RESTARTING) {
            if (!engineExited) {
                engineExited = true;
                startActivity(new android.content.Intent(this, RestartActivity.class)
                        .putExtra("enginePid", android.os.Process.myPid())
                        .putExtra("restartMode", selectedStartMode));
            }
            return;
        }
        String text;
        switch (state) {
            case NativeBridge.SESSION_CREATED:
                text = "Direct native host: created";
                break;
            case NativeBridge.SESSION_BOOTING:
                text = "Direct native host: booting";
                break;
            case NativeBridge.SESSION_RUNNING:
                text = "Direct native host: running";
                break;
            case NativeBridge.SESSION_STOPPING:
                text = "Direct native host: stopping";
                break;
            case NativeBridge.SESSION_STOPPED:
                text = "Direct native host: stopped";
                break;
            case NativeBridge.SESSION_FAILED:
                text = "Direct native host: failed";
                break;
            default:
                text = "Direct native host: state " + state;
                break;
        }
        // The app stops the session only in onDestroy, so a stop seen earlier is the engine exiting to the desktop.
        if ((state == NativeBridge.SESSION_STOPPING || state == NativeBridge.SESSION_STOPPED) && !engineExited) {
            engineExited = true;
            finishAndRemoveTask();
            return;
        }
        boolean failed = state == NativeBridge.SESSION_FAILED && error != null && !error.isEmpty();
        if (failed) {
            text += "\n" + error;
            title.error(error.split("\n", 2)[0]);
        }
        statusView.setText(text);
        // Diagnostics only: a running game shows nothing over the picture.
        statusView.setVisibility(failed ? View.VISIBLE : View.GONE);
        if (failed) statusView.bringToFront();
    }

    @Override public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (controls != null) controls.cancelAllTouches();
        withHandle(handle -> NativeBridge.focus(handle, hasFocus));
        if (hasFocus) {
            getWindow().getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        }
    }

    @Override protected void onPause() {
        if (controls != null) controls.cancelAllTouches();
        if (surface != null) surface.suspendNative();
        withHandle(handle -> NativeBridge.focus(handle, false));
        super.onPause();
    }

    @Override protected void onResume() {
        super.onResume();
        if (surface != null) surface.resumeNative();
        withHandle(handle -> NativeBridge.focus(handle, true));
    }

    @Override protected void onDestroy() {
        long handle;
        synchronized (lifecycleLock) {
            destroyed = true;
            handle = nativeHandle;
            nativeHandle = 0L;
        }
        statusHandler.removeCallbacks(statusPoll);
        statusHandler.removeCallbacks(loadingPoll);
        if (title != null) title.stop();
        if (controls != null) controls.cancelAllTouches();
        if (surface != null) surface.clearNativeHandle();
        if (handle != 0L) {
            NativeBridge.focus(handle, false);
            NativeBridge.stop(handle, STOP_TIMEOUT_MS);
            NativeBridge.destroy(handle);
            NativeBridge.discardSession(handle);
        }
        startupExecutor.shutdownNow();
        super.onDestroy();
        // The engine keeps process-wide state; the next launch starts it in a fresh process.
        if (engineExited) android.os.Process.killProcess(android.os.Process.myPid());
    }
}
