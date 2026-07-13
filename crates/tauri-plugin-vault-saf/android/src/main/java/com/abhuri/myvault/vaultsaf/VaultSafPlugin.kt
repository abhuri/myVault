package com.abhuri.myvault.vaultsaf

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.ByteArrayOutputStream
import java.io.FileOutputStream
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.ArrayDeque
import java.util.concurrent.atomic.AtomicBoolean

@InvokeArg
internal class PathArgs { lateinit var path: String }

@InvokeArg
internal class SaveArgs {
    lateinit var path: String
    lateinit var text: String
    lateinit var expectedRevisionHex: String
    var expectedByteLen: Long = -1
}

private data class Child(val documentId: String, val name: String, val mime: String, val size: Long)
private data class PendingDirectory(val documentId: String, val path: String, val depth: Int)

internal fun isProtectedRootName(name: String): Boolean = name == ".trash" || name == ".obsidian"

// Rust cursors compare UTF-8 bytes. Kotlin's natural String order compares
// UTF-16 code units, which differs for some supplementary Unicode characters.
internal fun comparePortablePathsUtf8(left: String, right: String): Int {
    val leftBytes = left.toByteArray(StandardCharsets.UTF_8)
    val rightBytes = right.toByteArray(StandardCharsets.UTF_8)
    val sharedLength = minOf(leftBytes.size, rightBytes.size)
    for (index in 0 until sharedLength) {
        val difference = (leftBytes[index].toInt() and 0xff) - (rightBytes[index].toInt() and 0xff)
        if (difference != 0) return difference
    }
    return leftBytes.size.compareTo(rightBytes.size)
}

@TauriPlugin
class VaultSafPlugin(private val activity: Activity) : Plugin(activity) {
    private val resolver = activity.applicationContext.contentResolver
    private val preferences = activity.applicationContext.getSharedPreferences("myvault-saf", 0)
    private val operationInFlight = AtomicBoolean(false)
    private val ioLock = Any()

    @Command
    fun status(invoke: Invoke) {
        val response = JSObject()
        response.put("active", persistedRoot() != null)
        invoke.resolve(response)
    }

    @Command
    fun chooseRoot(invoke: Invoke) {
        if (!operationInFlight.compareAndSet(false, true)) {
            invoke.reject("Another folder selection is active", "PICKER_BUSY")
            return
        }
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
            addFlags(Intent.FLAG_GRANT_PREFIX_URI_PERMISSION)
        }
        try {
            startActivityForResult(invoke, intent, "rootResult")
        } catch (_: Exception) {
            operationInFlight.set(false)
            invoke.reject("Folder picker could not be opened", "PICKER_UNAVAILABLE")
        }
    }

    @ActivityCallback
    fun rootResult(invoke: Invoke, result: ActivityResult) {
        operationInFlight.set(false)
        if (result.resultCode == Activity.RESULT_CANCELED) {
            val response = JSObject()
            response.put("outcome", "cancelled")
            invoke.resolve(response)
            return
        }
        val uri = result.data?.data
        if (result.resultCode != Activity.RESULT_OK || uri == null) {
            invoke.reject("Folder selection failed", "PICKER_FAILED")
            return
        }
        val flags = result.data!!.flags and
            (Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        if (flags and Intent.FLAG_GRANT_READ_URI_PERMISSION == 0 ||
            flags and Intent.FLAG_GRANT_WRITE_URI_PERMISSION == 0) {
            invoke.reject("The selected folder did not grant read and write access", "PICKER_PERMISSION")
            return
        }
        val previousRoot = persistedRoot()
        val permissionAlreadyPersisted = hasPersistedReadWritePermission(uri)
        var newlyAcquiredPermission = false
        var activationCommitted = false
        try {
            resolver.takePersistableUriPermission(uri, flags)
            newlyAcquiredPermission = !permissionAlreadyPersisted
            // Validate that the tree root is queryable before replacing the old capability.
            DocumentsContract.getTreeDocumentId(uri)
            queryChildren(uri, DocumentsContract.getTreeDocumentId(uri))
            if (!preferences.edit().putString(ROOT_KEY, uri.toString()).commit()) {
                throw IllegalStateException("root preference was not persisted")
            }
            activationCommitted = true
            if (previousRoot != null && previousRoot != uri) releasePermissionQuietly(previousRoot)
            val response = JSObject()
            response.put("outcome", "activated")
            invoke.resolve(response)
        } catch (_: Exception) {
            if (newlyAcquiredPermission && !activationCommitted) releasePermissionQuietly(uri)
            invoke.reject("The selected folder could not be activated", "PICKER_FAILED")
        }
    }

    @Command
    fun inventory(invoke: Invoke) {
        try {
            val response = synchronized(ioLock) { buildInventory(requireRoot()) }
            invoke.resolve(response)
        } catch (_: SecurityException) {
            clearRoot()
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: LimitException) {
            invoke.reject("Vault inventory exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: Exception) {
            invoke.reject("Vault inventory is unavailable", "VAULT_UNAVAILABLE")
        }
    }

    @Command
    fun readNote(invoke: Invoke) {
        val path = try { canonicalNotePath(invoke.parseArgs(PathArgs::class.java).path) }
        catch (_: Exception) {
            invoke.reject("Invalid note path", "INVALID_PATH")
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot()
                val document = resolveDocument(root, path) ?: throw MissingException()
                if (document.mime == DocumentsContract.Document.MIME_TYPE_DIR) throw MissingException()
                noteResponse(readBounded(root, document.documentId))
            }
            invoke.resolve(response)
        } catch (_: MissingException) {
            invoke.reject("Note was not found", "NOTE_NOT_FOUND")
        } catch (_: CharacterCodingException) {
            invoke.reject("Note is not valid UTF-8", "NOTE_NOT_UTF8")
        } catch (_: LimitException) {
            invoke.reject("Note exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: SecurityException) {
            clearRoot()
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: Exception) {
            invoke.reject("Note is unavailable", "VAULT_UNAVAILABLE")
        }
    }

    @Command
    fun saveNote(invoke: Invoke) {
        val args = try { invoke.parseArgs(SaveArgs::class.java) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val path = try { canonicalNotePath(args.path) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val replacement = args.text.toByteArray(StandardCharsets.UTF_8)
        if (replacement.size > MAX_NOTE_BYTES || !REVISION.matches(args.expectedRevisionHex) || args.expectedByteLen < 0) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot()
                val document = resolveDocument(root, path) ?: throw MissingException()
                val current = readBounded(root, document.documentId)
                if (current.size.toLong() != args.expectedByteLen || digest(current) != args.expectedRevisionHex) {
                    throw StaleException()
                }
                val uri = DocumentsContract.buildDocumentUriUsingTree(root, document.documentId)
                // SAF has no portable atomic compare-and-swap or atomic replace.
                // This guarded in-place write can still race an external writer;
                // post-write verification only establishes the bytes seen here.
                resolver.openFileDescriptor(uri, "rwt")?.use { descriptor ->
                    FileOutputStream(descriptor.fileDescriptor).use { output ->
                        output.write(replacement)
                        output.flush()
                        descriptor.fileDescriptor.sync()
                    }
                } ?: throw MissingException()
                val verify = readBounded(root, document.documentId)
                if (!verify.contentEquals(replacement)) throw UnknownWriteException()
                noteResponse(verify).apply { put("outcome", "saved") }
            }
            invoke.resolve(response)
        } catch (_: StaleException) {
            invoke.resolve(saveOutcome("staleRevision"))
        } catch (_: UnknownWriteException) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: MissingException) {
            invoke.resolve(saveOutcome("noteNotFound"))
        } catch (_: SecurityException) {
            clearRoot()
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: Exception) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        }
    }

    private fun buildInventory(root: Uri): JSObject {
        val queue = ArrayDeque<PendingDirectory>()
        queue.add(PendingDirectory(DocumentsContract.getTreeDocumentId(root), "", 0))
        val entries = mutableListOf<JSObject>()
        var scanned = 0
        while (queue.isNotEmpty()) {
            val directory = queue.removeFirst()
            for (child in queryChildren(root, directory.documentId)) {
                scanned += 1
                if (scanned > MAX_ENTRIES) throw LimitException()
                if (child.name.isEmpty() || child.name == "." || child.name == ".." || child.name.contains('/') || child.name.contains('\\') || child.name.contains('\u0000')) continue
                val path = if (directory.path.isEmpty()) child.name else "${directory.path}/${child.name}"
                if (directory.path.isEmpty() && isProtectedRootName(child.name)) continue
                if (child.mime == DocumentsContract.Document.MIME_TYPE_DIR) {
                    if (directory.depth + 1 > MAX_DEPTH) throw LimitException()
                    queue.add(PendingDirectory(child.documentId, path, directory.depth + 1))
                } else {
                    val entry = JSObject()
                    entry.put("path", path)
                    entry.put("kind", if (path.endsWith(".md", true)) "markdown" else "file")
                    entry.put("byteLen", child.size.coerceAtLeast(0))
                    entries.add(entry)
                }
            }
        }
        entries.sortWith(Comparator { left, right ->
            comparePortablePathsUtf8(left.getString("path"), right.getString("path"))
        })
        val array = JSArray()
        entries.forEach(array::put)
        return JSObject().apply {
            put("entries", array)
            put("scannedEntries", scanned)
        }
    }

    private fun queryChildren(root: Uri, parentId: String): List<Child> {
        val uri = DocumentsContract.buildChildDocumentsUriUsingTree(root, parentId)
        val projection = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_SIZE,
        )
        val children = mutableListOf<Child>()
        resolver.query(uri, projection, null, null, null)?.use { cursor ->
            while (cursor.moveToNext()) {
                children.add(Child(cursor.getString(0), cursor.getString(1) ?: "", cursor.getString(2) ?: "", if (cursor.isNull(3)) 0 else cursor.getLong(3)))
            }
        } ?: throw SecurityException("query failed")
        return children
    }

    private fun resolveDocument(root: Uri, path: String): Child? {
        var parentId = DocumentsContract.getTreeDocumentId(root)
        var selected: Child? = null
        for (component in path.split('/')) {
            val matches = queryChildren(root, parentId).filter { it.name == component }
            if (matches.size != 1) return null
            selected = matches.single()
            parentId = selected.documentId
        }
        return selected
    }

    private fun readBounded(root: Uri, documentId: String): ByteArray {
        val uri = DocumentsContract.buildDocumentUriUsingTree(root, documentId)
        resolver.openInputStream(uri)?.use { input ->
            val output = ByteArrayOutputStream()
            val buffer = ByteArray(8192)
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                if (output.size() + count > MAX_NOTE_BYTES) throw LimitException()
                output.write(buffer, 0, count)
            }
            return output.toByteArray()
        }
        throw MissingException()
    }

    private fun noteResponse(bytes: ByteArray): JSObject {
        val text = try {
            StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(bytes)).toString()
        } catch (_: java.nio.charset.CharacterCodingException) { throw CharacterCodingException() }
        return JSObject().apply {
            put("text", text)
            put("revisionHex", digest(bytes))
            put("byteLen", bytes.size.toLong())
        }
    }

    private fun saveOutcome(outcome: String): JSObject = JSObject().apply { put("outcome", outcome) }

    private fun canonicalNotePath(value: String): String {
        if (value.isEmpty() || value.startsWith('/') || value.contains('\\') || value.contains('\u0000')) throw IllegalArgumentException()
        val parts = value.split('/')
        if (parts.size > MAX_DEPTH || parts.any { it.isEmpty() || it == "." || it == ".." }) throw IllegalArgumentException()
        if (!value.endsWith(".md", true) || isProtectedRootName(parts.first())) throw IllegalArgumentException()
        return parts.joinToString("/")
    }

    private fun hasPersistedReadWritePermission(uri: Uri): Boolean =
        resolver.persistedUriPermissions.any {
            it.uri == uri && it.isReadPermission && it.isWritePermission
        }

    private fun releasePermissionQuietly(uri: Uri) {
        try {
            resolver.releasePersistableUriPermission(
                uri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
            )
        } catch (_: Exception) {
            // Best effort only: activation state already has an authoritative root.
        }
    }

    private fun persistedRoot(): Uri? {
        val value = preferences.getString(ROOT_KEY, null) ?: return null
        val uri = try { Uri.parse(value) } catch (_: Exception) { clearRoot(); return null }
        if (!hasPersistedReadWritePermission(uri)) { clearRoot(); return null }
        return uri
    }

    private fun requireRoot(): Uri = persistedRoot() ?: throw SecurityException("no root")
    private fun clearRoot() { preferences.edit().remove(ROOT_KEY).commit() }
    private fun digest(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

    private class MissingException : Exception()
    private class LimitException : Exception()
    private class StaleException : Exception()
    private class UnknownWriteException : Exception()
    private class CharacterCodingException : Exception()

    companion object {
        private const val ROOT_KEY = "root-uri"
        private const val MAX_NOTE_BYTES = 16 * 1024 * 1024
        private const val MAX_ENTRIES = 5000
        private const val MAX_DEPTH = 64
        private val REVISION = Regex("^[0-9a-f]{64}$")
    }
}
