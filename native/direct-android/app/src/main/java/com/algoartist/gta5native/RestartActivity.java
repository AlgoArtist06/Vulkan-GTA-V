package com.algoartist.gta5native;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Process;

/** Relaunches in a fresh process because the engine keeps process-wide state. */
public final class RestartActivity extends Activity {
    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        int enginePid = getIntent().getIntExtra("enginePid", -1);
        if (enginePid <= 0 || enginePid == Process.myPid()) {
            finish();
            return;
        }
        int mode = getIntent().getIntExtra("restartMode", 0);
        Process.killProcess(enginePid);
        startActivity(new Intent(this, MainActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK)
                .putExtra("restartMode", mode));
        finish();
        Process.killProcess(Process.myPid());
    }
}
