package com.algoartist.gta5native;

import java.util.HashMap;
import java.util.Iterator;
import java.util.Map;

/**
 * Keeps each engine key and mouse button down for a few presented frames.
 *
 * <p>The engine polls key state once per frame, so a tap released between two
 * polls never reached it (adb taps and quick key presses at low frame rates).</p>
 */
final class FrameHold implements InputLedger.Sink {
    /** Input is sampled when a frame's update starts and presented a frame later:
     *  the third present after a press ends a frame whose update saw it. */
    static final long FRAMES = 3;
    /** Releases are not withheld longer than this while no frames are presented. */
    static final long MAX_HOLD_MS = 500;
    private static final int BUTTON_BASE = 256;

    private final InputLedger.Sink sink;
    private long frames;
    private long now;
    /** Held actions: the frame and uptime of their press. */
    private final Map<Integer, long[]> pressedAt = new HashMap<>();
    /** Released actions still reported down. */
    private final Map<Integer, Boolean> releasing = new HashMap<>();

    FrameHold(InputLedger.Sink sink) {
        if (sink == null) {
            throw new IllegalArgumentException("FrameHold requires a sink");
        }
        this.sink = sink;
    }

    @Override public int key(int vk, boolean down) {
        return change(vk, down);
    }

    @Override public int button(int button, boolean down) {
        return change(BUTTON_BASE + button, down);
    }

    /** The engine presented `presents` frames by uptime `uptimeMs`: send releases that are due. */
    int frame(long presents, long uptimeMs) {
        frames = presents;
        now = uptimeMs;
        int result = 0;
        for (Iterator<Integer> it = releasing.keySet().iterator(); it.hasNext(); ) {
            int action = it.next();
            if (due(action)) {
                it.remove();
                result = firstError(result, release(action));
            }
        }
        return result;
    }

    /** Sends every withheld release now (focus loss, release all). */
    int flush() {
        int result = 0;
        for (int action : releasing.keySet()) {
            result = firstError(result, release(action));
        }
        releasing.clear();
        return result;
    }

    private int change(int action, boolean down) {
        if (down) {
            pressedAt.put(action, new long[] { frames, now });
            // A press while the previous release is withheld continues that press.
            if (releasing.remove(action) != null) {
                return 0;
            }
            return send(action, true);
        }
        if (!pressedAt.containsKey(action) || due(action)) {
            return release(action);
        }
        releasing.put(action, Boolean.TRUE);
        return 0;
    }

    private boolean due(int action) {
        long[] at = pressedAt.get(action);
        return at == null || frames >= at[0] + FRAMES || now - at[1] >= MAX_HOLD_MS;
    }

    private int release(int action) {
        pressedAt.remove(action);
        return send(action, false);
    }

    private int send(int action, boolean down) {
        return action >= BUTTON_BASE ? sink.button(action - BUTTON_BASE, down) : sink.key(action, down);
    }

    private static int firstError(int current, int next) {
        return current != 0 ? current : next;
    }
}
