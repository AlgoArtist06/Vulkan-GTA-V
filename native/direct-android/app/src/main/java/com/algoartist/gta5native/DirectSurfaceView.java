package com.algoartist.gta5native;

import android.content.Context;
import android.util.AttributeSet;
import android.util.Log;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;

/**
 * Forwards the Android Surface lifecycle to the direct native renderer.
 * All methods run on the UI thread, and the native side owns any
 * ANativeWindow reference after ATTACH returns.
 */
public final class DirectSurfaceView extends SurfaceView implements SurfaceHolder.Callback2 {
    private static final String TAG = "Gta5Direct";
    private long nativeHandle;
    private boolean surfaceReady;
    private boolean surfaceAttached;

    public DirectSurfaceView(Context context) {
        super(context);
        init();
    }

    public DirectSurfaceView(Context context, AttributeSet attrs) {
        super(context, attrs);
        init();
    }

    public DirectSurfaceView(Context context, AttributeSet attrs, int defStyleAttr) {
        super(context, attrs, defStyleAttr);
        init();
    }

    private void init() {
        getHolder().addCallback(this);
        // MainActivity owns key input; see TouchControllerView on focus and the first key.
        setFocusable(false);
    }

    public void setNativeHandle(long handle) {
        if (nativeHandle == handle) {
            attachIfReady();
            return;
        }
        detachIfNeeded();
        nativeHandle = handle;
        attachIfReady();
    }

    public void clearNativeHandle() {
        detachIfNeeded();
        nativeHandle = 0L;
    }

    public boolean isSurfaceReady() {
        return surfaceReady;
    }

    /** Pauses native presentation while keeping the Android Surface attached. */
    public void suspendNative() {
        if (nativeHandle == 0L || !surfaceAttached) {
            return;
        }
        int result = NativeBridge.surface(nativeHandle, getHolder().getSurface(), getWidth(),
                getHeight(), NativeBridge.SURFACE_SUSPEND);
        if (result != 0) {
            Log.w(TAG, "native surface suspend failed: " + result);
        }
    }

    /** Resumes native presentation after an activity pause. */
    public void resumeNative() {
        if (nativeHandle == 0L || !surfaceAttached || !surfaceReady) {
            return;
        }
        int result = NativeBridge.surface(nativeHandle, getHolder().getSurface(), getWidth(),
                getHeight(), NativeBridge.SURFACE_RESUME);
        if (result != 0) {
            Log.w(TAG, "native surface resume failed: " + result);
        }
    }

    @Override public void surfaceCreated(SurfaceHolder holder) {
        surfaceReady = true;
        attachIfReady();
    }

    @Override public void surfaceChanged(SurfaceHolder holder, int format,
                                         int width, int height) {
        if (nativeHandle != 0L && surfaceReady && width > 0 && height > 0) {
            int result = NativeBridge.surface(nativeHandle, holder.getSurface(), width,
                    height, NativeBridge.SURFACE_RESIZE);
            if (result != 0) {
                Log.w(TAG, "native surface resize failed: " + result);
            }
        }
    }

    @Override public void surfaceDestroyed(SurfaceHolder holder) {
        surfaceReady = false;
        detachIfNeeded();
    }

    @Override public void surfaceRedrawNeeded(SurfaceHolder holder) {
        // Rendering is driven by the native engine. No Java canvas is used.
    }

    @Override public void surfaceRedrawNeededAsync(SurfaceHolder holder,
                                                    Runnable drawingFinished) {
        try {
            surfaceRedrawNeeded(holder);
        } finally {
            drawingFinished.run();
        }
    }

    private void attachIfReady() {
        if (nativeHandle == 0L || !surfaceReady || surfaceAttached) {
            return;
        }
        Surface surface = getHolder().getSurface();
        if (surface == null || !surface.isValid()) {
            return;
        }
        int result = NativeBridge.surface(nativeHandle, surface, getWidth(), getHeight(),
                NativeBridge.SURFACE_ATTACH);
        if (result != 0) {
            Log.w(TAG, "native surface attach failed: " + result);
        } else {
            surfaceAttached = true;
        }
    }

    private void detachIfNeeded() {
        if (nativeHandle == 0L || !surfaceAttached) {
            return;
        }
        int result = NativeBridge.surface(nativeHandle, null, 0, 0,
                NativeBridge.SURFACE_DETACH);
        if (result != 0) {
            Log.w(TAG, "native surface detach failed: " + result);
        }
        surfaceAttached = false;
    }
}
