package com.abhuri.myvault.privateroot

import android.app.Activity
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
            val response = JSObject()
            response.put("path", root.canonicalPath)
            invoke.resolve(response)
        } catch (_: Exception) {
            invoke.reject("No-backup directory could not be resolved", "NO_BACKUP_UNAVAILABLE")
        }
    }
}
