package com.algoartist.gta5native;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Canvas;
import android.graphics.Matrix;
import android.graphics.Paint;
import android.graphics.RectF;
import android.graphics.Typeface;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Random;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The browser build's title screen (mirror index.html and game.js), ported to
 * a native view: the game's LOADINGSCREEN_STARTUP art on its 1280x720 stage,
 * the Story Mode / Sandbox Mode choice in the button bar, and the loading
 * progress. Assets are the web build's title/ files (stage_title.py).
 */
final class TitleScreen extends View {
    interface Listener {
        /** 1 = Story Mode, 2 = Sandbox Mode on the GTA V map, 3 = Sandbox Mode on env_test. */
        void startMode(int mode);
    }

    private static final class Def {
        final String tex;
        final float w, h, x, y, yr, x1, yr1, xr1;
        Def(String tex, float w, float h, float x, float y, float yr, float x1, float yr1, float xr1) {
            this.tex = tex; this.w = w; this.h = h; this.x = x; this.y = y; this.yr = yr;
            this.x1 = x1; this.yr1 = yr1; this.xr1 = xr1;
        }
    }

    private static Def bg(int n, float yr, float toYr, float toXr) { return bg(n, yr, toYr, toXr, 98); }
    private static Def bg(int n, float yr, float toYr, float toXr, float x) {
        return new Def("ls" + n + "_background", 1024, 512, x, 95, yr, x, toYr, toXr);
    }
    private static Def fg(int n, String name, float w, float h, float x, float y, float toX) { return fg(n, name, w, h, x, y, toX, 0); }
    private static Def fg(int n, String name, float w, float h, float x, float y, float toX, float yr) {
        return new Def("ls" + n + "_" + name, w, h, x, y, yr, toX, yr, 0);
    }

    // The movie's loadingScreens array: setup and tweenTo of every layer, bottom to top.
    private static final Def[][] SCREENS = {
        { new Def("beach_bg", 1024, 512, 98, 95, 20, 98, -6, 5), new Def("beach_fg", 343, 629, 480, 95, 0, 720, 0, 0) },
        null,
        { bg(0, 7, -7, 0), fg(0, "foreground", 299, 599, 360, 140, 560, 7) },
        { bg(1, 6, -12, 0), fg(1, "foreground", 503, 206, 300, 465, 480), fg(1, "foreground_franklin", 368, 588, 440, 140, 745) },
        { bg(2, 7, -5, 5), fg(2, "foreground", 311, 558, 745, 200, 450), fg(2, "foreground_chop", 320, 512, 640, 260, 310) },
        { bg(3, 12, -4, 0, 110), fg(3, "foreground", 412, 609, 440, 120, 730) },
        { bg(4, 7, -4, 5), fg(4, "foreground", 251, 643, 400, 85, 250) },
        { bg(5, 7, -6, 0), fg(5, "foreground", 389, 640, 450, 95, 700) },
        { bg(6, 7, 0, 5), fg(6, "foreground", 367, 603, 410, 115, 800) },
        { bg(7, 6, -6, 5), fg(7, "foreground", 311, 625, 860, 95, 520) },
        { bg(8, 7, -5, 5), fg(8, "foreground", 492, 497, 400, 190, 660) },
        { bg(9, 7, -3, 5), fg(9, "foreground", 331, 675, 220, 50, 140) },
        { bg(10, 6, -5, 5), fg(10, "foreground", 352, 682, 545, 65, 485) },
        { bg(11, 7, -3, 5), fg(11, "foreground", 344, 669, 309, 51, 424) },
        { bg(12, 6, -4, 0), fg(12, "foreground_michael", 462, 677, 345, 42, 220) },
        { bg(13, 5, -6, 5), fg(13, "foreground", 399, 681, 430, 39, 250) },
        { bg(14, 6, -2, 5), fg(14, "foreground", 392, 631, 650, 95, 850) },
        { bg(15, 6, -3, 5), fg(15, "foreground", 385, 642, 404, 78, 261) },
        { bg(16, 6, -3, 5), fg(16, "foreground", 300, 600, 300, 150, 400) },
    };
    private static final int[][] ORDERS = {
        { 0, 16, 18, 3, 12, 2, 17, 10, 9, 8, 15, 6, 11, 14, 4, 7, 13 }, { 0, 4, 6, 17, 11, 16, 18, 7, 9, 15, 3, 14, 10, 13, 2, 8, 12 },
        { 0, 7, 16, 14, 13, 3, 10, 2, 15, 18, 8, 9, 4, 17, 12, 6, 11 }, { 0, 8, 18, 9, 13, 4, 11, 14, 7, 17, 6, 12, 3, 16, 2, 15, 10 },
        { 0, 11, 12, 7, 18, 14, 9, 13, 4, 2, 15, 11, 8, 16, 6, 3, 17 }, { 0, 10, 16, 2, 13, 17, 8, 9, 18, 3, 12, 6, 7, 4, 14, 15, 10 },
    };
    private static final float PERSPECTIVE = 1229f;

    private static final class Layer {
        final Def def;
        final Bitmap bitmap;
        float x, yr, xr;
        Layer(Def def, Bitmap bitmap) { this.def = def; this.bitmap = bitmap; x = def.x; yr = def.yr; xr = 0; }
    }

    private static final class Screen {
        final List<Layer> layers;
        float slideFrom, slideTo, x;
        long start, settled;
        Screen(List<Layer> layers) { this.layers = layers; }
    }

    // Button bar: text, key cap, Android key codes (the web's ACTS).
    private static final String[] ACTS = { "back", "map5", "map6", "sandbox", "story" };
    private static final String[] ACT_TEXT = { "Back", "GTA V Map", "GTA VI Map", "Sandbox Mode", "Story Mode" };
    private static final String[] ACT_KEY = { "Backspace", "5", "6", "Space", "Enter" };
    private static final int[][] ACT_CODES = {
        { KeyEvent.KEYCODE_DEL }, { KeyEvent.KEYCODE_5, KeyEvent.KEYCODE_NUMPAD_5 }, { KeyEvent.KEYCODE_6, KeyEvent.KEYCODE_NUMPAD_6 },
        { KeyEvent.KEYCODE_SPACE }, { KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER },
    };
    private static final String[] LOADING = { "", "Loading Story Mode", "Loading Sandbox Mode: GTA V Map", "Loading Sandbox Mode: GTA VI Map" };

    // game.js progress(): engine log lines that mark the loading stages.
    private static final Pattern[] STAGE_PATTERNS = {
        Pattern.compile("^Audio rpf ="), Pattern.compile("Using Settings"), Pattern.compile("created D3D11"),
        Pattern.compile("CShaderLib::Init"), Pattern.compile("LOAD_ADDITIONAL_TEXT - finished"), Pattern.compile("Starting audio thread"),
        Pattern.compile("Loading singleplayer metadata"), Pattern.compile("^Loading SRL file"), Pattern.compile("Removing prologue IPL light groups"),
    };
    private static final int[] STAGE_PCT = { 24, 38, 45, 50, 56, 62, 66, 72, 84 };
    private static final String[] STAGE_LABEL = { "Mounting archives", "Reading settings", "Starting graphics", "Loading shaders",
        "Loading text", "Starting audio", "Loading audio metadata", "Starting scripts", "Loading the world" };
    private static final Pattern START_MODE = Pattern.compile("^\\[wasm\\] start mode: (waiting|story|sandbox on env_test|sandbox|none)");
    private static final double WORLD_BYTES = 1.2e9;

    private final Listener listener;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private final ExecutorService decoder = Executors.newSingleThreadExecutor();
    private final int[] order = ORDERS[new Random().nextInt(ORDERS.length)];
    private final float density;
    private final Bitmap logo, spinner;
    private final Paint bitmapPaint = new Paint(Paint.FILTER_BITMAP_FLAG | Paint.ANTI_ALIAS_FLAG);
    private final Paint fill = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint text = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint key = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Matrix matrix = new Matrix();
    private final float[] src = new float[8], dst = new float[8];
    private final RectF rect = new RectF();
    private final RectF[] hit = new RectF[ACTS.length];

    private Screen active, leaving;
    private List<Layer> next;
    private int index, alt = -1;
    private boolean started, stopped;

    // the start choice
    private int startMode;
    private boolean startCommitted, pickingMap, waitingChoice, asking;

    // loading progress
    private double target, shown;
    private boolean ready, loadingGone, chose;
    private int sent;
    private long dataBytes, worldBase = -1, lastActivity = SystemClock.uptimeMillis(), stallLogged;
    private String label = "Starting...", lastLabel = "";
    private int labelColor = 0xffffffff;

    TitleScreen(Context context, Listener listener) {
        super(context);
        this.listener = listener;
        density = context.getResources().getDisplayMetrics().density;
        logo = asset("title/logo.png");
        spinner = asset("title/spinner.png");
        Typeface chalet = Typeface.createFromAsset(context.getAssets(), "title/chalet.ttf");
        text.setTypeface(chalet);
        text.setTextSize(12);
        key.setTypeface(chalet);
        key.setTextSize(10);
        setBackgroundColor(0xff000000);
        setVisibility(GONE);
        handler.postDelayed(this::progressTick, 50);
        handler.postDelayed(this::stallTick, 2000);
    }

    private Bitmap asset(String path) {
        try (InputStream stream = getContext().getAssets().open(path)) {
            Bitmap bitmap = BitmapFactory.decodeStream(stream);
            if (bitmap == null) throw new IllegalStateException("cannot decode " + path);
            return bitmap;
        } catch (IOException error) {
            throw new IllegalStateException("missing title asset " + path, error);
        }
    }

    /** Shows the title screen and starts its screens (the web page shows it from the start). */
    void begin() {
        if (started || stopped) return;
        started = true;
        setVisibility(VISIBLE);
        prepare(0);
        handler.postDelayed(this::cycle, 500);
        postInvalidateOnAnimation();
    }

    // a screen's images, decoded before it may show
    private void prepare(int i) {
        Def[] defs = SCREENS[order[i]];
        decoder.execute(() -> {
            List<Layer> layers = new ArrayList<>();
            for (Def def : defs) layers.add(new Layer(def, asset("title/art/" + def.tex + ".webp")));
            handler.post(() -> { if (stopped) recycle(layers); else next = layers; });
        });
    }

    private void cycle() {
        if (stopped) return;
        if (next == null) { handler.postDelayed(this::cycle, 16); return; }
        show(next);
        next = null;
        index = (index + 1) % order.length;
        prepare(index);
        handler.postDelayed(this::cycle, 14000);
    }

    private void show(List<Layer> layers) {
        long now = SystemClock.uptimeMillis();
        alt = alt <= 0 ? 1 : -1;
        Screen screen = new Screen(layers);
        screen.slideFrom = alt * 1280;
        screen.x = screen.slideFrom;
        screen.start = now;
        leaving = active;
        if (leaving != null) { leaving.slideFrom = 0; leaving.slideTo = -alt * 1280; leaving.start = now; }
        active = screen;
        screen.slideTo = 0;
    }

    private static float cubicInOut(float t) { return t < 0.5f ? 4 * t * t * t : 1 - (float) Math.pow(2 - 2 * t, 3) / 2; }

    private void tick(long now) {
        for (Screen screen : new Screen[] { leaving, active }) {
            if (screen == null) continue;
            float p = Math.min(1, (now - screen.start) / 850f);
            screen.x = screen.slideFrom + (screen.slideTo - screen.slideFrom) * cubicInOut(p);
            if (screen == active && p == 1 && screen.settled == 0) screen.settled = now;
            if (screen.settled != 0) {
                float q = Math.min(1, (now - screen.settled) / 42000f);
                for (Layer layer : screen.layers) {
                    layer.x = layer.def.x + (layer.def.x1 - layer.def.x) * q;
                    layer.yr = layer.def.yr + (layer.def.yr1 - layer.def.yr) * q;
                    layer.xr = layer.def.xr1 * q;
                }
            }
            if (screen == leaving && p == 1) leaving = null;
        }
    }

    // CSS translate3d(x, y, 0) rotateY(-yr) rotateX(-xr) about the layer's top left, projected by perspective 1229px around the stage centre.
    private void drawLayer(Canvas canvas, Layer layer, float slide) {
        double b = Math.toRadians(-layer.yr), a = Math.toRadians(-layer.xr);
        float w = layer.def.w, h = layer.def.h;
        float[] u = { 0, w, w, 0 }, v = { 0, 0, h, h };
        for (int i = 0; i < 4; i++) {
            double y1 = v[i] * Math.cos(a), z1 = v[i] * Math.sin(a);
            double x2 = u[i] * Math.cos(b) + z1 * Math.sin(b), z2 = -u[i] * Math.sin(b) + z1 * Math.cos(b);
            double s = PERSPECTIVE / (PERSPECTIVE - z2);
            dst[i * 2] = (float) (640 + (x2 + layer.x + slide - 640) * s);
            dst[i * 2 + 1] = (float) (360 + (y1 + layer.def.y - 360) * s);
        }
        float bw = layer.bitmap.getWidth(), bh = layer.bitmap.getHeight();
        src[0] = 0; src[1] = 0; src[2] = bw; src[3] = 0; src[4] = bw; src[5] = bh; src[6] = 0; src[7] = bh;
        matrix.setPolyToPoly(src, 0, dst, 0, 4);
        canvas.drawBitmap(layer.bitmap, matrix, bitmapPaint);
    }

    @Override protected void onDraw(Canvas canvas) {
        long now = SystemClock.uptimeMillis();
        tick(now);
        float width = getWidth(), height = getHeight();
        float s = Math.min(width / 1280f, height / 720f);
        canvas.save();
        canvas.translate((width - 1280 * s) / 2, (height - 720 * s) / 2);
        canvas.scale(s, s);
        canvas.clipRect(0, 0, 1280, 720);
        for (Screen screen : new Screen[] { leaving, active }) {
            if (screen == null) continue;
            for (Layer layer : screen.layers) drawLayer(canvas, layer, screen.x);
        }
        rect.set(95, 492.5f, 95 + 186, 492.5f + 186);
        canvas.drawBitmap(logo, null, rect, bitmapPaint);
        drawBars(canvas, now);
        canvas.restore();
        drawProgress(canvas, width, height);
        if (!stopped) postInvalidateOnAnimation();
    }

    private boolean loadingItem(int act) {
        return !pickingMap && (act == 4 ? startMode == 1 : act == 3 && startMode >= 2);
    }

    private boolean hidden(int act) {
        if (!waitingChoice && !startCommitted) return true;
        boolean mode = act == 3 || act == 4;
        return startCommitted ? !loadingItem(act) : pickingMap == mode;
    }

    private float keyWidth(String cap) {
        return 4 + Math.max(12, key.measureText(cap.toUpperCase(Locale.ROOT))) + 8;
    }

    private float buttonWidth(int act) {
        return loadingItem(act) ? text.measureText(LOADING[startMode]) + 24 : text.measureText(ACT_TEXT[act]) + 12;
    }

    private void drawBars(Canvas canvas, long now) {
        List<Integer> visible = new ArrayList<>();
        for (int act = 0; act < ACTS.length; act++) if (!hidden(act) && !loadingItem(act)) visible.add(act);
        for (int act = 0; act < ACTS.length; act++) if (!hidden(act) && loadingItem(act)) visible.add(act);
        float modesWidth = 12;
        for (int i = 0; i < visible.size(); i++) modesWidth += buttonWidth(visible.get(i)) + (i > 0 ? 8 : 0);
        String pct = Math.round(shown) + "%";
        text.setFontFeatureSettings("tnum");
        float pctWidth = text.measureText(pct);
        text.setFontFeatureSettings(null);
        float progressWidth = 12 + text.measureText(label) + 15 + pctWidth + 24;
        float x = 1184 - (modesWidth + 3 + progressWidth), top = 642;

        bar(canvas, x, top, modesWidth);
        float cx = x + 6;
        for (int act = 0; act < ACTS.length; act++) hit[act] = null;
        for (int act : visible) {
            float w = buttonWidth(act);
            hit[act] = new RectF(cx, top, cx + w, top + 24);
            if (loadingItem(act)) {
                cx = label(canvas, LOADING[startMode], cx, top, 0xffffffff, false);
                cx = spin(canvas, cx, top, now);
            } else {
                cx = label(canvas, ACT_TEXT[act], cx, top, 0xffffffff, false);
            }
            cx += 8;
        }

        float px = x + modesWidth + 3;
        bar(canvas, px, top, progressWidth);
        px = label(canvas, label, px + 6, top, labelColor, true) + 15;
        text.setFontFeatureSettings("tnum");
        px = label(canvas, pct, px, top, 0xffffffff, false);
        text.setFontFeatureSettings(null);
        spin(canvas, px, top, now);
    }

    private void bar(Canvas canvas, float x, float top, float width) {
        fill.setColor(0xb4000000);
        rect.set(x, top, x + width, top + 24);
        canvas.drawRoundRect(rect, 2.4f, 2.4f, fill);
    }

    private float label(Canvas canvas, String value, float x, float top, int color, boolean shadow) {
        text.setColor(color);
        if (shadow) text.setShadowLayer(1, 0.85f, 0.85f, 0x73000000); else text.clearShadowLayer();
        Paint.FontMetrics metrics = text.getFontMetrics();
        canvas.drawText(value, x, top + 12 - (metrics.ascent + metrics.descent) / 2, text);
        text.clearShadowLayer();
        return x + text.measureText(value);
    }

    private float keyCap(Canvas canvas, String cap, float x, float top, float alpha) {
        String upper = cap.toUpperCase(Locale.ROOT);
        float left = x + 4, width = Math.max(12, key.measureText(upper)) + 8, capTop = top + 4.5f;
        int layer = canvas.saveLayerAlpha(left, capTop, left + width, capTop + 15, Math.round(alpha * 255));
        fill.setColor(0xffffffff);
        rect.set(left, capTop, left + width, capTop + 15);
        canvas.drawRoundRect(rect, 2, 2, fill);
        key.setColor(0xff000000);
        Paint.FontMetrics metrics = key.getFontMetrics();
        canvas.drawText(upper, left + (width - key.measureText(upper)) / 2, capTop + 8 - (metrics.ascent + metrics.descent) / 2, key);
        canvas.restoreToCount(layer);
        return left + width;
    }

    // .ls-spin: 2 s, steps(60), rotate(0 -> -360deg) scaleX(-1)
    private float spin(Canvas canvas, float x, float top, long now) {
        float cx = x + 2 + 11, cy = top + 12;
        float step = (float) Math.floor((now % 2000) / 2000.0 * 60) / 60;
        canvas.save();
        canvas.translate(cx, cy);
        canvas.rotate(-360 * step);
        canvas.scale(-1, 1);
        rect.set(-12, -12, 12, 12);
        canvas.drawBitmap(spinner, null, rect, bitmapPaint);
        canvas.restore();
        return x + 24;
    }

    // @keyframes ls-ask { 50% { opacity: .35 } }, 1.2 s ease-in-out
    private static float askOpacity(long now) {
        float p = (now % 1200) / 1200f;
        return p < 0.5f ? 1 - 0.65f * ease(p * 2) : 0.35f + 0.65f * ease(p * 2 - 1);
    }

    // cubic-bezier(.42, 0, .58, 1)
    private static float ease(float x) {
        float t = x;
        for (int i = 0; i < 8; i++) {
            float bx = 3 * 0.42f * t * (1 - t) * (1 - t) + 3 * 0.58f * t * t * (1 - t) + t * t * t - x;
            float d = 3 * 0.42f * (1 - t) * (1 - 3 * t) + 3 * 0.58f * t * (2 - 3 * t) + 3 * t * t;
            if (Math.abs(d) < 1e-6f) break;
            t = Math.max(0, Math.min(1, t - bx / d));
        }
        return 3 * t * t * (1 - t) + t * t * t;
    }

    private void drawProgress(Canvas canvas, float width, float height) {
        fill.setColor(0x14ffffff);
        canvas.drawRect(0, height - 3 * density, width, height, fill);
        fill.setColor(0xff7cc576);
        canvas.drawRect(0, height - 3 * density, (float) (width * Math.min(100, shown) / 100), height, fill);
    }

    private void act(int act) {
        if (!waitingChoice || startCommitted) return;
        if (act == 4) choose(1);
        else if (act == 1) choose(2);
        else if (act == 2) choose(3);
        else { pickingMap = act == 3; showModes(); }
    }

    private void choose(int mode) {
        pickingMap = false;
        if (mode != startMode) {
            startMode = mode;
            listener.startMode(mode);
        }
        showModes();
    }

    private void showModes() {
        if (startMode != 0) asking = false;
        if (waitingChoice && startMode == 0) label = pickingMap ? "Choose a map to continue" : "Choose Story Mode or Sandbox Mode to continue";
        invalidate();
    }

    @Override public boolean onTouchEvent(MotionEvent event) {
        if (!waitingChoice) return true;
        if (event.getActionMasked() != MotionEvent.ACTION_UP) return true;
        float width = getWidth(), height = getHeight(), s = Math.min(width / 1280f, height / 720f);
        float x = (event.getX() - (width - 1280 * s) / 2) / s, y = (event.getY() - (height - 720 * s) / 2) / s;
        for (int act = 0; act < ACTS.length; act++) {
            if (hit[act] != null && !startCommitted && hit[act].contains(x, y)) { act(act); break; }
        }
        return true;
    }

    /** The page's keydown handler: a key of a visible button acts on it and does not reach the game. */
    boolean onKey(KeyEvent event) {
        if (!started || !waitingChoice || startCommitted || loadingGone || event.isCtrlPressed() || event.isAltPressed() || event.isMetaPressed()) return false;
        for (int act = 0; act < ACTS.length; act++) {
            if (hidden(act)) continue;
            for (int code : ACT_CODES[act]) {
                if (code != event.getKeyCode()) continue;
                if (event.getAction() == KeyEvent.ACTION_DOWN) act(act);
                return true;
            }
        }
        return false;
    }

    /** game.js Module.print/printErr: progress(s) and consoleLine(s) for each engine output line. */
    void line(String s) {
        lastActivity = SystemClock.uptimeMillis();
        if (!chose && s.startsWith("[wasm] start mode: ")) {
            Matcher m = START_MODE.matcher(s);
            if (!m.find()) return;
            if (m.group(1).equals("waiting")) {
                if (startMode == 0) { waitingChoice = true; asking = true; showModes(); }
            } else {
                chose = true;
                int mode = switch (m.group(1)) { case "story" -> 1; case "sandbox" -> 2; case "sandbox on env_test" -> 3; default -> 0; };
                startCommitted = true; pickingMap = false;
                if (mode != 0) startMode = mode;
                showModes();
            }
            return;
        }
        for (int i = 0; i < STAGE_PATTERNS.length && sent < STAGE_PCT[STAGE_PCT.length - 1]; i++) {
            if (STAGE_PCT[i] > sent && STAGE_PATTERNS[i].matcher(s).find()) { sent = STAGE_PCT[i]; progress(STAGE_PCT[i], STAGE_LABEL[i]); }
        }
    }

    private void progress(double pct, String value) {
        lastActivity = SystemClock.uptimeMillis();
        if (pct <= target) return;
        target = pct;
        if (value != null) { lastLabel = value; if (!waitingChoice) label = value; }
        if (pct >= 72 && worldBase < 0) worldBase = dataBytes;
    }

    /** Total game data bytes the engine has loaded (the web's {dl} reports). */
    void dataTotal(long total) {
        long delta = total - dataBytes;
        if (delta <= 0) return;
        lastActivity = SystemClock.uptimeMillis();
        dataBytes = total;
    }

    /** The GPU worker's first present. */
    void firstFrame() { progress(76, "First frame"); }

    /** The GPU worker's "world shown": the title screen fades out. */
    void world() {
        lastActivity = SystemClock.uptimeMillis();
        progress(100, "Ready");
        ready = true;
    }

    void error(String message) {
        label = message;
        labelColor = 0xffee6666;
        invalidate();
    }

    private void progressTick() {
        if (loadingGone) return;
        if (worldBase >= 0 && !ready) target = Math.max(target, 72 + 26 * Math.min(1, (dataBytes - worldBase) / WORLD_BYTES));
        double cap = ready ? 100 : Math.min(99, target + 6);
        double goal = ready ? 100 : (shown < target ? target : cap);
        shown += (goal - shown) * (goal == target || ready ? 0.12 : 0.004);
        if (ready && shown > 99.5) {
            loadingGone = true;
            animate().alpha(0).setDuration(800);
            handler.postDelayed(this::stop, 1000);
            return;
        }
        if (!started) { handler.postDelayed(this::progressTick, 50); return; }
        invalidate();
        handler.postDelayed(this::progressTick, 50);
    }

    // Stall watchdog: nothing from the engine for 30 s while loading.
    private void stallTick() {
        if (ready || stopped) return;
        long now = SystemClock.uptimeMillis();
        if (waitingChoice) {
            if (startMode == 0) { lastActivity = now; handler.postDelayed(this::stallTick, 2000); return; }
            waitingChoice = false;
            label = lastLabel;
        }
        long quiet = (now - lastActivity) / 1000;
        if (quiet > 30) {
            label = "No progress for " + quiet + " s" + (lastLabel.isEmpty() ? "" : " (last step: " + lastLabel + ")") + ": the device may be out of memory. Close other apps, or restart the game.";
            labelColor = 0xffee9966;
            stallLogged = quiet;
        } else if (stallLogged != 0) {
            stallLogged = 0;
            labelColor = 0xffffffff;
        }
        handler.postDelayed(this::stallTick, 2000);
    }

    void stop() {
        stopped = true;
        loadingGone = true;
        handler.removeCallbacksAndMessages(null);
        decoder.shutdownNow();
        setVisibility(GONE);
        // Nothing shows the loading art again: detach the view and free its decoded bitmaps (~25 MB).
        if (getParent() instanceof ViewGroup) ((ViewGroup) getParent()).removeView(this);
        for (Screen screen : new Screen[] { leaving, active }) if (screen != null) recycle(screen.layers);
        if (next != null) recycle(next);
        active = leaving = null;
        next = null;
        logo.recycle();
        spinner.recycle();
    }

    private static void recycle(List<Layer> layers) {
        for (Layer layer : layers) layer.bitmap.recycle();
    }
}
