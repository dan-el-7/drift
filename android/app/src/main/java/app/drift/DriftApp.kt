package app.drift

import android.app.Activity
import android.app.Application
import android.os.Bundle

class DriftApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Drift.init(this)
        registerActivityLifecycleCallbacks(object : ActivityLifecycleCallbacks {
            override fun onActivityResumed(a: Activity) { inFront = true }
            override fun onActivityPaused(a: Activity) { inFront = false }
            override fun onActivityCreated(a: Activity, b: Bundle?) {}
            override fun onActivityStarted(a: Activity) {}
            override fun onActivityStopped(a: Activity) {}
            override fun onActivitySaveInstanceState(a: Activity, b: Bundle) {}
            override fun onActivityDestroyed(a: Activity) {}
        })
    }

    companion object {
        @Volatile var inFront = false
    }
}
