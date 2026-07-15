package com.abhuri.myvault.privateroot

import android.app.Activity
import android.system.Os
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@TauriPlugin
class PrivateRootPlugin(private val activity: Activity) : Plugin(activity) {
    @Command
    fun noBackupRoot(invoke: Invoke) {
        val root = activity.applicationContext.noBackupFilesDir
        if (!root.isDirectory) {
            invoke.reject("No-backup directory is unavailable", "NO_BACKUP_UNAVAILABLE")
            return
        }
        try {
            // Android commonly creates this app-private directory as 0771.
            // The Rust capability boundary deliberately requires exact 0700,
            // so tighten the native-proven root before returning its path.
            Os.chmod(root.canonicalPath, 0x1C0)
            val response = JSObject()
            response.put("path", root.canonicalPath)
            invoke.resolve(response)
        } catch (_: Exception) {
            invoke.reject("No-backup directory could not be resolved", "NO_BACKUP_UNAVAILABLE")
        }
    }
}
