package com.algoartist.gta5native;

import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Canvas;
import android.graphics.Insets;
import android.graphics.Paint;
import android.graphics.RectF;
import android.graphics.Typeface;
import android.os.SystemClock;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowInsets;

/**
 * Touch gamepad. The engine gets a virtual Xbox-style controller (rage::ioPad 0), so GTA V
 * picks its own on-foot, vehicle, aircraft and menu controls and its prompts name the buttons
 * shown here. Left: a floating move stick. Right: swipe to look (the right stick). Shoulder
 * triggers also look while held, so a thumb can fire and turn; a double tap latches LT.
 */
public final class TouchControllerView extends View {
    /** Native input consumed by the Android host/JNI bridge. */
    public interface Sink {
        /**
         * The whole pad: {@code buttons} in rage::ioPad bit order, {@code sticks} as
         * LX | LY << 8 | RX << 16 | RY << 24 (128 = centre, 0 = left/up), {@code triggers} as LT | RT << 8.
         */
        void pad(int buttons, int sticks, int triggers);

        /** Release all input during focus loss or host teardown. */
        void releaseAll();
    }

    // rage::ioPad button bits.
    static final int L2 = 1, R2 = 1 << 1, L1 = 1 << 2, R1 = 1 << 3, Y = 1 << 4, B = 1 << 5, A = 1 << 6, X = 1 << 7,
            BACK = 1 << 8, L3 = 1 << 9, R3 = 1 << 10, START = 1 << 11, UP = 1 << 12, RIGHT = 1 << 13, DOWN = 1 << 14, LEFT = 1 << 15;
    /** A tap shorter than this is held until the engine (20 to 60 fps) has sampled it. */
    private static final long MIN_PRESS_MS = 90L;
    private static final long LATCH_TAP_MS = 300L;
    /** The look stick returns to centre when the finger stops for this long. */
    private static final long LOOK_IDLE_MS = 45L;

    private enum Shape { ROUND, PILL, SMALL }

    /** A pressable control and the pad bit it holds. */
    private static final class Control {
        final String label;
        final int bit;
        final Shape shape;
        final int color;
        final boolean looks;
        final RectF bounds = new RectF();

        Control(String label, int bit, Shape shape, int color, boolean looks) {
            this.label = label;
            this.bit = bit;
            this.shape = shape;
            this.color = color;
            this.looks = looks;
        }
    }

    private static final int WHITE = 0xffffffff;
    private final Control buttonA = new Control("A", A, Shape.ROUND, 0xff6fd36f, false);
    private final Control buttonB = new Control("B", B, Shape.ROUND, 0xffff6b5e, false);
    private final Control buttonX = new Control("X", X, Shape.ROUND, 0xff5fa8ff, false);
    private final Control buttonY = new Control("Y", Y, Shape.ROUND, 0xffffd04a, false);
    private final Control triggerL = new Control("LT", L2, Shape.PILL, WHITE, true);
    private final Control triggerR = new Control("RT", R2, Shape.PILL, WHITE, true);
    private final Control bumperL = new Control("LB", L1, Shape.PILL, WHITE, false);
    private final Control bumperR = new Control("RB", R1, Shape.PILL, WHITE, false);
    private final Control stickL = new Control("L3", L3, Shape.SMALL, WHITE, false);
    private final Control stickR = new Control("R3", R3, Shape.SMALL, WHITE, false);
    private final Control back = new Control("VIEW", BACK, Shape.PILL, WHITE, false);
    private final Control start = new Control("MENU", START, Shape.PILL, WHITE, false);
    private final Control[] controls = { buttonA, buttonB, buttonX, buttonY, triggerL, triggerR, bumperL, bumperR, stickL, stickR, back, start };
    private static final int[] DPAD_BITS = { UP, DOWN, LEFT, RIGHT };
    private static final String[] RADIAL = { "SIZE +", "SIZE -", "OPACITY", "LOOK SPEED" };

    private static final int OWN_NONE = 0, OWN_STICK = 1, OWN_LOOK = 2, OWN_DPAD = 3, OWN_BUTTON = 4, OWN_RADIAL = 5;
    private static final int MAX_POINTERS = 32;

    private final Sink sink;
    private final SharedPreferences settings;
    private final Paint fill = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint stroke = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint text = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final float density;
    private final RectF stickZone = new RectF(), dpad = new RectF(), more = new RectF(), scratch = new RectF();
    private final int[] owner = new int[MAX_POINTERS];
    private final Control[] pressed = new Control[MAX_POINTERS];
    private final int[] dpadBit = new int[MAX_POINTERS];
    private final float[] lastX = new float[MAX_POINTERS], lastY = new float[MAX_POINTERS];
    private final long[] lastMove = new long[MAX_POINTERS];
    /** Press time of each held bit; releases wait for MIN_PRESS_MS. */
    private final long[] pressedAt = new long[16];
    private final Runnable[] pendingRelease = new Runnable[16];
    private float scale, opacity, lookSpeed;
    private int insetLeft, insetTop, insetRight, insetBottom;
    private int buttons, sentButtons = -1, sentSticks = -1, sentTriggers = -1;
    private boolean latchL;
    private long lastLTapUp;
    // the floating move stick
    private int stickPointer = -1;
    private float stickCx, stickCy, stickX, stickY;
    // the look stick: finger speed in stick units, decaying when the finger rests
    private int lookPointer = -1;
    /** While a selection wheel is open the look finger is a held stick, measured from where it was when the wheel opened. */
    private boolean anchored;
    private float anchorX, anchorY;
    private float lookX, lookY;
    private final Runnable lookIdle = new Runnable() {
        @Override public void run() {
            if (anchored) return;
            if (lookPointer >= 0 && SystemClock.uptimeMillis() - lastMove[lookPointer] < LOOK_IDLE_MS) {
                postDelayed(this, LOOK_IDLE_MS);
                return;
            }
            lookX = lookY = 0;
            send();
        }
    };
    private int radialPointer = -1, radialSelected = -1;

    public TouchControllerView(Context context, Sink sink) {
        super(context);
        this.sink = sink;
        density = getResources().getDisplayMetrics().density;
        settings = context.getSharedPreferences("touch-controls", Context.MODE_PRIVATE);
        scale = settings.getFloat("scale", 1.0f);
        opacity = settings.getFloat("opacity", 0.75f);
        lookSpeed = settings.getFloat("look", 1.0f);
        // Keys go through MainActivity. A focusable view here would take focus when
        // a physical key leaves touch mode, and Android drops that first key.
        setFocusable(false);
        text.setTypeface(Typeface.create(Typeface.DEFAULT, Typeface.BOLD));
        text.setTextAlign(Paint.Align.CENTER);
        stroke.setStyle(Paint.Style.STROKE);
    }

    /** Releases every active pointer and the whole pad. MainActivity calls this on pause and focus loss. */
    public void cancelAllTouches() { releaseAllPointers(); }

    @Override public WindowInsets onApplyWindowInsets(WindowInsets insets) {
        Insets safe = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
        insetLeft = safe.left;
        insetTop = safe.top;
        insetRight = safe.right;
        insetBottom = safe.bottom;
        invalidate();
        return insets;
    }

    private float dp(float value) { return value * density; }

    private float stickRadius() { return dp(56) * scale; }

    private void layoutControls() {
        float w = getWidth(), h = getHeight(), s = scale;
        float left = insetLeft + dp(14), right = w - insetRight - dp(14), bottom = h - insetBottom - dp(12), top = insetTop + dp(8);
        // face buttons, bottom right
        float spacing = dp(52) * s, cx = right - dp(78) * s, cy = bottom - dp(72) * s;
        round(buttonA, cx, cy + spacing, dp(25) * s);
        round(buttonB, cx + spacing, cy, dp(25) * s);
        round(buttonX, cx - spacing, cy, dp(25) * s);
        round(buttonY, cx, cy - spacing, dp(25) * s);
        round(stickR, cx - spacing * 2.15f, cy + spacing * 0.55f, dp(19) * s);
        // Shoulders on the outer columns, clear of the game's help box (x 0.686..0.86 of the width)
        // and its FPS counter in the top-left corner. Triggers sit lower, under the thumbs.
        float bumperW = dp(72) * s, bumperH = dp(38) * s, triggerW = dp(92) * s, triggerH = dp(56) * s;
        float shoulderTop = top + dp(30);
        bumperL.bounds.set(left, shoulderTop, left + bumperW, shoulderTop + bumperH);
        bumperR.bounds.set(right - bumperW, shoulderTop, right, shoulderTop + bumperH);
        float triggerTop = bumperL.bounds.bottom + dp(14) * s;
        triggerL.bounds.set(left, triggerTop, left + triggerW, triggerTop + triggerH);
        triggerR.bounds.set(right - triggerW, triggerTop, right, triggerTop + triggerH);
        // move stick: floating in the lower left, resting above the game's radar
        float r = stickRadius(), restCy = h * 0.76f - r - dp(6) * s;
        stickZone.set(insetLeft, triggerL.bounds.bottom + dp(8), w * 0.42f, h);
        if (stickPointer < 0) { stickCx = left + r + dp(10) * s; stickCy = restCy; }
        round(stickL, left + r * 2 + dp(22) * s, restCy - r + dp(4) * s, dp(19) * s);
        float pad = dp(96) * s;
        dpad.set(left + r * 2 + dp(44) * s, bottom - pad, left + r * 2 + dp(44) * s + pad, bottom);
        // VIEW, MENU and MORE: one row at the top, left of the help box
        float rowH = dp(34) * s, smallW = dp(64) * s, gap = dp(14);
        float rowLeft = Math.max(bumperL.bounds.right + gap * 3, w * 0.30f);
        back.bounds.set(rowLeft, top, rowLeft + smallW, top + rowH);
        start.bounds.set(back.bounds.right + gap, top, back.bounds.right + gap + smallW, top + rowH);
        more.set(start.bounds.right + gap, top, start.bounds.right + gap + smallW, top + rowH);
    }

    private static void round(Control control, float cx, float cy, float radius) {
        control.bounds.set(cx - radius, cy - radius, cx + radius, cy + radius);
    }

    // ---- drawing ---------------------------------------------------------------------------------------

    @Override protected void onDraw(Canvas canvas) {
        layoutControls();
        int base = Math.round(255 * opacity);
        for (Control control : controls) if (!hidden(control)) drawControl(canvas, control, (buttons & control.bit) != 0, base);
        drawStick(canvas, base);
        drawDpad(canvas, base);
        drawPill(canvas, more, "MORE", radialPointer >= 0, base);
        if (radialPointer >= 0) drawRadial(canvas);
    }

    private void drawControl(Canvas canvas, Control control, boolean down, int base) {
        if (control.shape == Shape.PILL) {
            drawPill(canvas, control.bounds, control == triggerL && latchL ? "LT LOCK" : control.label, down, base);
            return;
        }
        RectF b = control.bounds;
        float r = b.width() / 2;
        fill.setColor(down ? control.color : 0xff10141a);
        fill.setAlpha(down ? Math.round(base * 0.9f) : Math.round(base * 0.55f));
        canvas.drawCircle(b.centerX(), b.centerY(), r, fill);
        stroke.setStrokeWidth(dp(control.shape == Shape.ROUND ? 2f : 1.5f));
        stroke.setColor(control.color);
        stroke.setAlpha(base);
        canvas.drawCircle(b.centerX(), b.centerY(), r, stroke);
        text.setColor(down ? 0xff000000 : control.color);
        text.setAlpha(Math.max(base, 170));
        text.setTextSize(control.shape == Shape.ROUND ? r * 0.85f : r * 0.6f);
        canvas.drawText(control.label, b.centerX(), b.centerY() + text.getTextSize() * 0.36f, text);
    }

    private void drawPill(Canvas canvas, RectF b, String label, boolean down, int base) {
        float radius = Math.min(b.height(), b.width()) / 2;
        fill.setColor(down ? 0xffffffff : 0xff10141a);
        fill.setAlpha(down ? Math.round(base * 0.85f) : Math.round(base * 0.55f));
        canvas.drawRoundRect(b, radius, radius, fill);
        stroke.setStrokeWidth(dp(1.2f));
        stroke.setColor(0xffffffff);
        stroke.setAlpha(Math.round(base * 0.8f));
        canvas.drawRoundRect(b, radius, radius, stroke);
        text.setColor(down ? 0xff000000 : 0xffffffff);
        text.setAlpha(Math.max(base, 170));
        text.setTextSize(Math.min(dp(13) * scale, b.height() * 0.4f));
        float room = b.width() - dp(8), width = text.measureText(label);
        if (width > room && room > 0) text.setTextSize(text.getTextSize() * room / width);
        canvas.drawText(label, b.centerX(), b.centerY() + text.getTextSize() * 0.35f, text);
    }

    private void drawStick(Canvas canvas, int base) {
        float r = stickRadius();
        fill.setColor(0xff10141a);
        fill.setAlpha(Math.round(base * 0.35f));
        canvas.drawCircle(stickCx, stickCy, r, fill);
        stroke.setStrokeWidth(dp(1.5f));
        stroke.setColor(0xffffffff);
        stroke.setAlpha(base);
        canvas.drawCircle(stickCx, stickCy, r, stroke);
        fill.setColor(0xffffffff);
        fill.setAlpha(Math.round(base * 0.6f));
        canvas.drawCircle(stickCx + stickX * r, stickCy + stickY * r, r * 0.42f, fill);
    }

    private void drawDpad(Canvas canvas, int base) {
        float cx = dpad.centerX(), cy = dpad.centerY(), arm = dpad.width() / 6;
        float[][] centers = { { cx, cy - arm * 2 }, { cx, cy + arm * 2 }, { cx - arm * 2, cy }, { cx + arm * 2, cy } };
        String[] arrows = { "▲", "▼", "◀", "▶" };
        for (int i = 0; i < 4; i++) {
            scratch.set(centers[i][0] - arm, centers[i][1] - arm, centers[i][0] + arm, centers[i][1] + arm);
            boolean down = (buttons & DPAD_BITS[i]) != 0;
            fill.setColor(down ? 0xffffffff : 0xff10141a);
            fill.setAlpha(Math.round(base * (down ? 0.85f : 0.55f)));
            canvas.drawRoundRect(scratch, dp(5), dp(5), fill);
            stroke.setStrokeWidth(dp(1.2f));
            stroke.setColor(0xffffffff);
            stroke.setAlpha(Math.round(base * 0.8f));
            canvas.drawRoundRect(scratch, dp(5), dp(5), stroke);
            text.setColor(down ? 0xff000000 : 0xffffffff);
            text.setAlpha(Math.max(base, 170));
            text.setTextSize(dp(12) * scale);
            canvas.drawText(arrows[i], centers[i][0], centers[i][1] + text.getTextSize() * 0.35f, text);
        }
    }

    private void drawRadial(Canvas canvas) {
        float cx = getWidth() / 2f, cy = getHeight() / 2f, radius = Math.min(getWidth(), getHeight()) * 0.3f;
        fill.setColor(0xff0a0e14);
        fill.setAlpha(Math.round(255 * 0.82f));
        canvas.drawCircle(cx, cy, radius * 1.35f, fill);
        text.setTextSize(dp(13));
        for (int i = 0; i < RADIAL.length; i++) {
            double angle = -Math.PI / 2 + i * 2 * Math.PI / RADIAL.length;
            float x = cx + (float) Math.cos(angle) * radius, y = cy + (float) Math.sin(angle) * radius;
            text.setColor(i == radialSelected ? 0xffffd04a : 0xffffffff);
            text.setAlpha(255);
            canvas.drawText(radialLabel(i), x, y + dp(4), text);
        }
        text.setColor(0xffbbbbbb);
        text.setTextSize(dp(10));
        canvas.drawText("slide to an item, release to choose", cx, cy + dp(4), text);
    }

    private String radialLabel(int i) {
        switch (RADIAL[i]) {
            case "OPACITY": return "OPACITY " + Math.round(opacity * 100) + "%";
            case "LOOK SPEED": return "LOOK x" + String.format(java.util.Locale.ROOT, "%.2f", lookSpeed);
            default: return RADIAL[i];
        }
    }

    // ---- touch -----------------------------------------------------------------------------------------

    @Override public boolean onTouchEvent(MotionEvent event) {
        int index = event.getActionIndex();
        switch (event.getActionMasked()) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                begin(event.getPointerId(index), event.getX(index), event.getY(index));
                break;
            case MotionEvent.ACTION_MOVE:
                for (int i = 0; i < event.getPointerCount(); i++) move(event.getPointerId(i), event.getX(i), event.getY(i));
                break;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_POINTER_UP:
                end(event.getPointerId(index), event.getX(index), event.getY(index));
                break;
            case MotionEvent.ACTION_CANCEL:
                releaseAllPointers();
                break;
            default:
                break;
        }
        send();
        invalidate();
        return true;
    }

    private void begin(int pointer, float x, float y) {
        if (pointer < 0 || pointer >= MAX_POINTERS || owner[pointer] != OWN_NONE) return;
        layoutControls();
        lastX[pointer] = x;
        lastY[pointer] = y;
        lastMove[pointer] = SystemClock.uptimeMillis();
        if (radialPointer >= 0) return;
        if (hit(more, x, y)) { owner[pointer] = OWN_RADIAL; radialPointer = pointer; radialSelected = -1; return; }
        for (Control control : controls) {
            if (hidden(control) || !hit(control, x, y)) continue;
            owner[pointer] = OWN_BUTTON;
            pressed[pointer] = control;
            if (control == triggerL && latchL) latchL = false;
            else press(control.bit, true);
            if (control.looks && lookPointer < 0) lookPointer = pointer;
            return;
        }
        if (dpad.contains(x, y)) { owner[pointer] = OWN_DPAD; dpadBit[pointer] = 0; updateDpad(pointer, x, y); return; }
        if (stickPointer < 0 && stickZone.contains(x, y)) {
            owner[pointer] = OWN_STICK;
            stickPointer = pointer;
            stickCx = x;
            stickCy = y;
            updateStick(x, y);
            return;
        }
        if (x >= getWidth() * 0.42f && lookPointer < 0) { owner[pointer] = OWN_LOOK; lookPointer = pointer; }
    }

    /** Round buttons take touches inside their circle (plus slop), so the gaps between them stay free. */
    private boolean hit(Control control, float x, float y) {
        RectF b = control.bounds;
        if (control.shape == Shape.PILL) return hit(b, x, y);
        return Math.hypot(x - b.centerX(), y - b.centerY()) <= b.width() / 2 + dp(6);
    }

    private boolean hit(RectF bounds, float x, float y) {
        float slop = dp(6);
        return x >= bounds.left - slop && x <= bounds.right + slop && y >= bounds.top - slop && y <= bounds.bottom + slop;
    }

    private void move(int pointer, float x, float y) {
        if (pointer < 0 || pointer >= MAX_POINTERS) return;
        switch (owner[pointer]) {
            case OWN_STICK: updateStick(x, y); break;
            case OWN_DPAD: updateDpad(pointer, x, y); break;
            case OWN_RADIAL: updateRadial(x, y); break;
            case OWN_LOOK:
            case OWN_BUTTON:
                if (pointer == lookPointer) look(pointer, x, y);
                break;
            default:
                break;
        }
    }

    /** Weapon wheel (LB) or character wheel (D-pad down) open: GTA V selects with a held right stick. */
    private boolean wheelOpen() { return (buttons & (L1 | DOWN)) != 0; }

    /** The character wheel opens in the lower right, under the face buttons: they hide and ignore touches meanwhile. */
    private boolean hidden(Control control) {
        return (buttons & DOWN) != 0 && (control == buttonA || control == buttonB || control == buttonX || control == buttonY || control == stickR);
    }

    /**
     * Swipe to look: finger speed sets the right stick; it eases back to centre when the finger rests.
     * With a wheel open the offset from the anchor sets it instead, held until the finger moves or lifts.
     */
    private void look(int pointer, float x, float y) {
        if (wheelOpen()) {
            if (!anchored) { anchored = true; anchorX = lastX[pointer]; anchorY = lastY[pointer]; }
            removeCallbacks(lookIdle);
            lookX = clamp((x - anchorX) / stickRadius());
            lookY = clamp((y - anchorY) / stickRadius());
            lastX[pointer] = x;
            lastY[pointer] = y;
            return;
        }
        anchored = false;
        long now = SystemClock.uptimeMillis();
        float dt = Math.max(8f, now - lastMove[pointer]);
        // dp per frame at 60 Hz; 4 dp per frame turns at full stick speed at LOOK x1.
        float k = lookSpeed / (dp(4) * dt / 16.7f);
        lookX = clamp((x - lastX[pointer]) * k);
        lookY = clamp((y - lastY[pointer]) * k);
        lastX[pointer] = x;
        lastY[pointer] = y;
        lastMove[pointer] = now;
        removeCallbacks(lookIdle);
        postDelayed(lookIdle, LOOK_IDLE_MS);
    }

    private static float clamp(float v) { return Math.max(-1f, Math.min(1f, v)); }

    private void end(int pointer, float x, float y) {
        if (pointer < 0 || pointer >= MAX_POINTERS) return;
        switch (owner[pointer]) {
            case OWN_STICK:
                stickPointer = -1;
                stickX = stickY = 0;
                break;
            case OWN_DPAD:
                setDpad(pointer, 0);
                break;
            case OWN_BUTTON: {
                Control control = pressed[pointer];
                pressed[pointer] = null;
                if (control == triggerL && (buttons & L2) != 0) {
                    long now = SystemClock.uptimeMillis();
                    // A second quick tap keeps LT held (aim) until LT is tapped again.
                    if (now - lastLTapUp < LATCH_TAP_MS) { latchL = true; lastLTapUp = 0; break; }
                    lastLTapUp = now;
                }
                if (control != null && !(control == triggerL && latchL) && !heldElsewhere(control)) press(control.bit, false);
                break;
            }
            case OWN_RADIAL:
                updateRadial(x, y);
                if (radialSelected >= 0) activateRadial(radialSelected);
                radialPointer = -1;
                radialSelected = -1;
                break;
            default:
                break;
        }
        if (pointer == lookPointer) {
            lookPointer = -1;
            anchored = false;
            removeCallbacks(lookIdle);
            lookX = lookY = 0;
        }
        owner[pointer] = OWN_NONE;
    }

    private boolean heldElsewhere(Control control) {
        for (Control other : pressed) if (other == control) return true;
        return false;
    }

    private void press(int bit, boolean down) {
        int index = Integer.numberOfTrailingZeros(bit);
        if (pendingRelease[index] != null) { removeCallbacks(pendingRelease[index]); pendingRelease[index] = null; }
        if (down) {
            buttons |= bit;
            pressedAt[index] = SystemClock.uptimeMillis();
            return;
        }
        long wait = MIN_PRESS_MS - (SystemClock.uptimeMillis() - pressedAt[index]);
        if (wait <= 0) { clear(bit); return; }
        pendingRelease[index] = () -> {
            pendingRelease[index] = null;
            clear(bit);
            send();
            invalidate();
        };
        postDelayed(pendingRelease[index], wait);
    }

    private void clear(int bit) {
        buttons &= ~bit;
        if (anchored && !wheelOpen()) { anchored = false; lookX = lookY = 0; }
    }

    private void updateStick(float x, float y) {
        float r = stickRadius();
        float ax = (x - stickCx) / r, ay = (y - stickCy) / r;
        float length = (float) Math.hypot(ax, ay);
        // the base follows a finger that runs past its edge
        if (length > 1f) {
            stickCx = x - ax / length * r;
            stickCy = y - ay / length * r;
            ax /= length;
            ay /= length;
        }
        stickX = ax;
        stickY = ay;
    }

    private void updateDpad(int pointer, float x, float y) {
        float dx = x - dpad.centerX(), dy = y - dpad.centerY(), dead = dpad.width() * 0.12f;
        int bit = 0;
        if (Math.max(Math.abs(dx), Math.abs(dy)) > dead) bit = Math.abs(dx) > Math.abs(dy) ? (dx < 0 ? LEFT : RIGHT) : (dy < 0 ? UP : DOWN);
        setDpad(pointer, bit);
    }

    private void setDpad(int pointer, int bit) {
        if (dpadBit[pointer] == bit) return;
        if (dpadBit[pointer] != 0) press(dpadBit[pointer], false);
        if (bit != 0) press(bit, true);
        dpadBit[pointer] = bit;
    }

    private static int axis(float v) { return Math.max(0, Math.min(255, Math.round(128 + v * 127))); }

    /** Sends the pad when it changed. */
    private void send() {
        int sticks = axis(stickX) | axis(stickY) << 8 | axis(lookX) << 16 | axis(lookY) << 24;
        int triggers = ((buttons & L2) != 0 ? 255 : 0) | ((buttons & R2) != 0 ? 255 << 8 : 0);
        if (buttons == sentButtons && sticks == sentSticks && triggers == sentTriggers) return;
        sentButtons = buttons;
        sentSticks = sticks;
        sentTriggers = triggers;
        sink.pad(buttons, sticks, triggers);
    }

    private void updateRadial(float x, float y) {
        float dx = x - getWidth() / 2f, dy = y - getHeight() / 2f;
        if (Math.hypot(dx, dy) < dp(50)) { radialSelected = -1; return; }
        double angle = Math.atan2(dy, dx) + Math.PI / 2 + Math.PI / RADIAL.length;
        if (angle < 0) angle += 2 * Math.PI;
        radialSelected = (int) (angle / (2 * Math.PI / RADIAL.length)) % RADIAL.length;
    }

    private void activateRadial(int item) {
        switch (RADIAL[item]) {
            case "SIZE +": scale = Math.min(1.5f, scale + 0.1f); save(); break;
            case "SIZE -": scale = Math.max(0.7f, scale - 0.1f); save(); break;
            case "OPACITY": opacity = opacity >= 0.95f ? 0.35f : Math.min(1f, opacity + 0.2f); save(); break;
            case "LOOK SPEED": lookSpeed = lookSpeed >= 2.0f ? 0.5f : lookSpeed + 0.25f; save(); break;
            default: break;
        }
    }

    private void save() {
        settings.edit().putFloat("scale", scale).putFloat("opacity", opacity).putFloat("look", lookSpeed).apply();
        invalidate();
    }

    @Override public void onWindowFocusChanged(boolean hasWindowFocus) {
        super.onWindowFocusChanged(hasWindowFocus);
        if (!hasWindowFocus) releaseAllPointers();
    }

    @Override protected void onDetachedFromWindow() {
        releaseAllPointers();
        super.onDetachedFromWindow();
    }

    private void releaseAllPointers() {
        for (int i = 0; i < pendingRelease.length; i++) {
            if (pendingRelease[i] != null) removeCallbacks(pendingRelease[i]);
            pendingRelease[i] = null;
        }
        removeCallbacks(lookIdle);
        java.util.Arrays.fill(owner, OWN_NONE);
        java.util.Arrays.fill(pressed, null);
        java.util.Arrays.fill(dpadBit, 0);
        buttons = 0;
        latchL = false;
        stickPointer = lookPointer = radialPointer = radialSelected = -1;
        anchored = false;
        stickX = stickY = lookX = lookY = 0;
        send();
        sink.releaseAll();
        invalidate();
    }
}
