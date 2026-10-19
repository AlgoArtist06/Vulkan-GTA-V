package com.algoartist.gta5native;

import android.view.Surface;

import java.util.HashMap;
import java.util.Map;

/**
 * Small JNI boundary for the direct C Android host.
 *
 * <p>The native library owns the engine session. Java only supplies the
 * Surface lifecycle and translates controller events into the shared input
 * ABI. There is no WebView, JavaScript runtime, or WASM VM here.</p>
 */
public final class NativeBridge {
    public static final int SESSION_CREATED = 0;
    public static final int SESSION_BOOTING = 1;
    public static final int SESSION_RUNNING = 2;
    public static final int SESSION_STOPPING = 3;
    public static final int SESSION_STOPPED = 4;
    public static final int SESSION_FAILED = 5;
    public static final int SESSION_RESTARTING = 6;

    /** Surface operation values shared with the JNI host. */
    public static final int SURFACE_SUSPEND = 3;
    public static final int SURFACE_RESUME = 4;
    public static final int SURFACE_DETACH = 5;
    public static final int SURFACE_ATTACH = 1;
    public static final int SURFACE_RESIZE = 2;

    /** Generic input event kinds shared with the JNI host. */
    public static final int INPUT_KEY = 1;
    public static final int INPUT_MOUSE_MOTION = 2;
    public static final int INPUT_MOUSE_BUTTON = 3;
    public static final int INPUT_MOUSE_WHEEL = 4;
    public static final int INPUT_FOCUS = 5;
    public static final int INPUT_RELEASE_ALL = 6;
    public static final int INPUT_TEXT_UTF16 = 7;
    public static final int INPUT_ABSOLUTE_MENU = 8;
    /** The title screen's start choice: 1 Story Mode, 2 Sandbox Mode, 3 Sandbox Mode on env_test. */
    public static final int INPUT_START_MODE = 9;
    /** The touch gamepad's whole state (native_host.c applies it to the engine's first pad). */
    public static final int INPUT_PAD = 10;

    /** High-bit source namespace reserved for physical Android keys. */
    public static final int PHYSICAL_KEY_SOURCE_BASE = 0x80000000;

    private static final Object SESSION_LOCK = new Object();
    private static final Map<Long, SessionInput> SESSION_INPUTS = new HashMap<>();

    static {
        System.loadLibrary("gta5_direct");
    }

    private NativeBridge() {}

    public static native long create(String assetsRoot, String savesRoot);

    public static native int boot(long handle);

    public static native int surface(long handle, Surface surface, int width,
                                     int height, int operation);

    public static native int status(long handle);

    public static native String error(long handle);

    public static native int input(long handle, int kind, int code, float value,
                                   float value2);

    public static native int stop(long handle, int timeoutMs);

    public static native void destroy(long handle);

    /**
     * Title-screen progress: fills out with { game data bytes loaded, frames
     * presented, world shown (0/1), pause menu open (0/1) } and returns the
     * engine output lines since the previous call, newline-separated.
     */
    public static native String loading(long handle, long[] out);

    public static int startMode(long handle, int mode) {
        return input(handle, INPUT_START_MODE, mode, 0.0f, 0.0f);
    }

    public static int key(long handle, int source, int vk, boolean down) {
        if (vk < 0 || vk > 255) {
            return -1;
        }
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            return session == null ? -1 : session.ledger.key(source, vk, down);
        }
    }

    public static int button(long handle, int source, int engineButton, boolean down) {
        if (engineButton < 0 || engineButton > 2) {
            return -1;
        }
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            return session == null ? -1
                    : session.ledger.button(source, engineButton, down);
        }
    }

    /**
     * Gamepad state: rage::ioPad button bits, sticks LX | LY << 8 | RX << 16 | RY << 24, triggers LT | RT << 8.
     * Three bytes travel in each float, which holds every integer below 2^24 exactly.
     */
    public static int pad(long handle, int buttons, int sticks, int triggers) {
        return input(handle, INPUT_PAD, buttons & 0xffff, sticks & 0xffffff,
                ((sticks >>> 24) | (triggers & 0xffff) << 8));
    }

    public static int motion(long handle, int dx, int dy) {
        return input(handle, INPUT_MOUSE_MOTION, 0, dx, dy);
    }

    public static int wheel(long handle, int notches) {
        return input(handle, INPUT_MOUSE_WHEEL, 0, notches, 0.0f);
    }

    public static int cancel(long handle, int source) {
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            return session == null ? -1 : session.ledger.cancel(source);
        }
    }

    public static int releaseAll(long handle) {
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            if (session == null) {
                return -1;
            }
            int result = firstError(session.ledger.clear(), session.hold.flush());
            return firstError(result, input(handle, INPUT_RELEASE_ALL, 0, 0.0f, 0.0f));
        }
    }

    public static int focus(long handle, boolean focused) {
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            if (session == null) {
                return -1;
            }
            int result = focused ? 0 : firstError(session.ledger.clear(), session.hold.flush());
            return firstError(result,
                    input(handle, INPUT_FOCUS, focused ? 1 : 0, 0.0f, 0.0f));
        }
    }

    /** The engine has presented `presents` frames: sends key releases held back for it. */
    public static int framePresented(long handle, long presents) {
        synchronized (SESSION_LOCK) {
            SessionInput session = sessionLocked(handle);
            return session == null ? -1 : session.hold.frame(presents, android.os.SystemClock.uptimeMillis());
        }
    }

    public static int textUtf16(long handle, int codeUnit) {
        return input(handle, INPUT_TEXT_UTF16, codeUnit & 0xffff, 0.0f, 0.0f);
    }

    public static int absoluteMenu(long handle, float x, float y) {
        return input(handle, INPUT_ABSOLUTE_MENU, 0, x, y);
    }

    /** Creates the input ledger as soon as a native session exists. */
    public static void registerSession(long handle) {
        synchronized (SESSION_LOCK) {
            sessionLocked(handle);
        }
    }

    /** Drops Java ownership state after the native session has been destroyed. */
    public static void discardSession(long handle) {
        synchronized (SESSION_LOCK) {
            SESSION_INPUTS.remove(handle);
        }
    }

    private static SessionInput sessionLocked(long handle) {
        if (handle == 0L) {
            return null;
        }
        Long key = handle;
        SessionInput session = SESSION_INPUTS.get(key);
        if (session == null) {
            session = new SessionInput(handle);
            SESSION_INPUTS.put(key, session);
        }
        return session;
    }

    private static int firstError(int current, int next) {
        return current != 0 ? current : next;
    }

    private static final class SessionInput {
        final FrameHold hold;
        final InputLedger ledger;

        SessionInput(long handle) {
            final long sessionHandle = handle;
            hold = new FrameHold(new InputLedger.Sink() {
                @Override public int key(int vk, boolean down) {
                    return input(sessionHandle, INPUT_KEY, vk,
                            down ? 1.0f : 0.0f, 0.0f);
                }

                @Override public int button(int button, boolean down) {
                    return input(sessionHandle, INPUT_MOUSE_BUTTON, button,
                            down ? 1.0f : 0.0f, 0.0f);
                }
            });
            ledger = new InputLedger(hold);
        }
    }
}
