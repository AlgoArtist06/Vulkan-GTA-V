package com.algoartist.gta5native;

import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;

/**
 * Per-session ownership ledger for keys and mouse buttons.
 *
 * <p>Several touch pointers or physical keys can hold the same engine action.
 * The sink sees one press when the first owner arrives and one release after
 * the last owner leaves. Repeated key-down events are idempotent.</p>
 */
final class InputLedger {
    interface Sink {
        int key(int vk, boolean down);

        int button(int button, boolean down);
    }

    private final Sink sink;
    private final Map<Integer, Set<Integer>> keysBySource = new HashMap<>();
    private final Map<Integer, Set<Integer>> buttonsBySource = new HashMap<>();
    private final Map<Integer, Integer> keyOwners = new HashMap<>();
    private final Map<Integer, Integer> buttonOwners = new HashMap<>();

    InputLedger(Sink sink) {
        if (sink == null) {
            throw new IllegalArgumentException("InputLedger requires a sink");
        }
        this.sink = sink;
    }

    int key(int source, int vk, boolean down) {
        if (vk < 0 || vk > 255) {
            return -1;
        }
        return update(source, vk, down, keysBySource, keyOwners, true);
    }

    int button(int source, int button, boolean down) {
        if (button < 0 || button > 2) {
            return -1;
        }
        return update(source, button, down, buttonsBySource, buttonOwners, false);
    }

    int cancel(int source) {
        int result = 0;
        result = firstError(result, releaseSource(source, keysBySource, keyOwners, true));
        result = firstError(result, releaseSource(source, buttonsBySource, buttonOwners, false));
        return result;
    }

    int clear() {
        int result = 0;
        for (Integer vk : keyOwners.keySet()) {
            result = firstError(result, sink.key(vk, false));
        }
        for (Integer button : buttonOwners.keySet()) {
            result = firstError(result, sink.button(button, false));
        }
        keysBySource.clear();
        buttonsBySource.clear();
        keyOwners.clear();
        buttonOwners.clear();
        return result;
    }

    boolean isEmpty() {
        return keyOwners.isEmpty() && buttonOwners.isEmpty();
    }

    private int update(int source, int action, boolean down,
                       Map<Integer, Set<Integer>> bySource,
                       Map<Integer, Integer> owners, boolean key) {
        Set<Integer> held = bySource.get(source);
        if (down) {
            if (held == null) {
                held = new HashSet<>();
                bySource.put(source, held);
            }
            if (!held.add(action)) {
                return 0;
            }
            int count = owners.getOrDefault(action, 0) + 1;
            owners.put(action, count);
            if (count == 1) {
                return key ? sink.key(action, true) : sink.button(action, true);
            }
            return 0;
        }

        if (held == null || !held.remove(action)) {
            return 0;
        }
        if (held.isEmpty()) {
            bySource.remove(source);
        }
        int count = owners.getOrDefault(action, 0);
        if (count <= 1) {
            owners.remove(action);
            return key ? sink.key(action, false) : sink.button(action, false);
        }
        owners.put(action, count - 1);
        return 0;
    }

    private int releaseSource(int source,
                              Map<Integer, Set<Integer>> bySource,
                              Map<Integer, Integer> owners,
                              boolean key) {
        Set<Integer> held = bySource.remove(source);
        if (held == null || held.isEmpty()) {
            return 0;
        }
        int result = 0;
        for (Integer action : held) {
            int count = owners.getOrDefault(action, 0);
            if (count <= 1) {
                owners.remove(action);
                result = firstError(result,
                        key ? sink.key(action, false) : sink.button(action, false));
            } else {
                owners.put(action, count - 1);
            }
        }
        return result;
    }

    private static int firstError(int current, int next) {
        return current != 0 ? current : next;
    }
}
