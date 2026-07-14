package com.abhuri.myvault.vaultsaf

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Base64
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
internal class RootArgs { lateinit var expectedRootIdentityHex: String }

@InvokeArg
internal class PathArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var path: String
}

@InvokeArg
internal class SaveArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var path: String
    lateinit var text: String
    lateinit var expectedRevisionHex: String
    var expectedByteLen: Long = -1
}

@InvokeArg
internal class BinaryWriteArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var path: String
    lateinit var bytesBase64: String
    lateinit var sha256Hex: String
    var byteLen: Long = -1
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

internal fun stableRootIdentityHex(rootUri: String): String = MessageDigest
    .getInstance("SHA-256")
    .digest((ROOT_IDENTITY_DOMAIN + rootUri).toByteArray(StandardCharsets.UTF_8))
    .joinToString("") { "%02x".format(it) }

@TauriPlugin
class VaultSafPlugin(private val activity: Activity) : Plugin(activity) {
    private val resolver = activity.applicationContext.contentResolver
    private val preferences = activity.applicationContext.getSharedPreferences("myvault-saf", 0)
    private val operationInFlight = AtomicBoolean(false)
    private val ioLock = Any()

    @Command
    fun status(invoke: Invoke) {
        val response = synchronized(ioLock) {
            try {
                val root = persistedRoot() ?: return@synchronized inactiveRootResponse()
                DocumentsContract.getTreeDocumentId(root)
                queryChildren(root, DocumentsContract.getTreeDocumentId(root))
                activeRootResponse(root)
            } catch (_: Exception) {
                clearRoot()
                inactiveRootResponse()
            }
        }
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
        val permissionAlreadyPersisted = hasPersistedReadWritePermission(uri)
        var newlyAcquiredPermission = false
        var activationCommitted = false
        try {
            resolver.takePersistableUriPermission(uri, flags)
            newlyAcquiredPermission = !permissionAlreadyPersisted
            val response = synchronized(ioLock) {
                val previousRoot = persistedRoot()
                // Validate that the tree root is queryable before replacing the old capability.
                DocumentsContract.getTreeDocumentId(uri)
                queryChildren(uri, DocumentsContract.getTreeDocumentId(uri))
                if (!preferences.edit().putString(ROOT_KEY, uri.toString()).commit()) {
                    throw IllegalStateException("root preference was not persisted")
                }
                activationCommitted = true
                if (previousRoot != null && previousRoot != uri) releasePermissionQuietly(previousRoot)
                JSObject().apply {
                    put("outcome", "activated")
                    put("rootIdentityHex", rootIdentity(uri))
                }
            }
            invoke.resolve(response)
        } catch (_: Exception) {
            if (newlyAcquiredPermission && !activationCommitted) releasePermissionQuietly(uri)
            invoke.reject("The selected folder could not be activated", "PICKER_FAILED")
        }
    }

    @Command
    fun inventory(invoke: Invoke) {
        val expectedRootIdentity = try { invoke.parseArgs(RootArgs::class.java).expectedRootIdentityHex }
        catch (_: Exception) {
            invoke.reject("Vault capability is invalid", "VAULT_UNAVAILABLE")
            return
        }
        try {
            val response = synchronized(ioLock) { buildInventory(requireRoot(expectedRootIdentity)) }
            invoke.resolve(response)
        } catch (_: RootMismatchException) {
            invoke.reject("Vault capability is stale", "VAULT_UNAVAILABLE")
        } catch (_: SecurityException) {
            clearRootIfMatches(expectedRootIdentity)
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: LimitException) {
            invoke.reject("Vault inventory exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: Exception) {
            invoke.reject("Vault inventory is unavailable", "VAULT_UNAVAILABLE")
        }
    }

    @Command
    fun readNote(invoke: Invoke) {
        val args = try { invoke.parseArgs(PathArgs::class.java) }
        catch (_: Exception) {
            invoke.reject("Invalid note path", "INVALID_PATH")
            return
        }
        val path = try { canonicalNotePath(args.path) }
        catch (_: Exception) {
            invoke.reject("Invalid note path", "INVALID_PATH")
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot(args.expectedRootIdentityHex)
                val document = resolveDocument(root, path) ?: throw MissingException()
                if (document.mime == DocumentsContract.Document.MIME_TYPE_DIR) throw MissingException()
                noteResponse(readBounded(root, document.documentId))
            }
            invoke.resolve(response)
        } catch (_: RootMismatchException) {
            invoke.reject("Vault capability is stale", "VAULT_UNAVAILABLE")
        } catch (_: MissingException) {
            invoke.reject("Note was not found", "NOTE_NOT_FOUND")
        } catch (_: CharacterCodingException) {
            invoke.reject("Note is not valid UTF-8", "NOTE_NOT_UTF8")
        } catch (_: LimitException) {
            invoke.reject("Note exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
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
                val root = requireRoot(args.expectedRootIdentityHex)
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
        } catch (_: RootMismatchException) {
            invoke.resolve(saveOutcome("vaultUnavailable"))
        } catch (_: StaleException) {
            invoke.resolve(saveOutcome("staleRevision"))
        } catch (_: UnknownWriteException) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: MissingException) {
            invoke.resolve(saveOutcome("noteNotFound"))
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: Exception) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        }
    }

    @Command
    fun readBinary(invoke: Invoke) {
        val args = try { invoke.parseArgs(PathArgs::class.java) }
        catch (_: Exception) {
            invoke.reject("Invalid transfer path", "INVALID_PATH")
            return
        }
        val path = try { canonicalPortablePath(args.path) }
        catch (_: Exception) {
            invoke.reject("Invalid transfer path", "INVALID_PATH")
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot(args.expectedRootIdentityHex)
                val document = resolveDocument(root, path) ?: throw MissingException()
                if (document.mime == DocumentsContract.Document.MIME_TYPE_DIR) throw MissingException()
                binaryResponse(readBounded(root, document.documentId))
            }
            invoke.resolve(response)
        } catch (_: RootMismatchException) {
            invoke.reject("Vault capability is stale", "VAULT_UNAVAILABLE")
        } catch (_: MissingException) {
            invoke.reject("Transfer object was not found", "NOTE_NOT_FOUND")
        } catch (_: LimitException) {
            invoke.reject("Transfer object exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: Exception) {
            invoke.reject("Transfer object is unavailable", "VAULT_UNAVAILABLE")
        }
    }

    @Command
    fun createBinary(invoke: Invoke) {
        writeBinary(invoke)
    }

    @Command
    fun replaceBinary(invoke: Invoke) {
        // SAF exposes neither atomic compare-and-swap nor a portable
        // evacuation/no-replace sequence. R2 therefore never mutates an
        // existing transfer target; R3 conflict handling owns that decision.
        invoke.resolve(saveOutcome("unsupportedReplace"))
    }

    private fun writeBinary(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinaryWriteArgs::class.java) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val path = try { canonicalPortablePath(args.path) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val replacement = try { Base64.decode(args.bytesBase64, Base64.NO_WRAP) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        if (replacement.size > MAX_TRANSFER_BYTES || replacement.size.toLong() != args.byteLen ||
            !REVISION.matches(args.sha256Hex) || digest(replacement) != args.sha256Hex) {
            invoke.resolve(saveOutcome("digestMismatch"))
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot(args.expectedRootIdentityHex)
                createBinaryDocument(root, path, replacement)
            }
            invoke.resolve(response)
        } catch (_: RootMismatchException) {
            invoke.resolve(saveOutcome("vaultUnavailable"))
        } catch (_: ExistingException) {
            invoke.resolve(saveOutcome("alreadyExists"))
        } catch (_: MissingException) {
            invoke.resolve(saveOutcome("notFound"))
        } catch (_: LimitException) {
            invoke.resolve(saveOutcome("resourceLimit"))
        } catch (_: UnknownWriteException) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: Exception) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        }
    }

    private fun createBinaryDocument(root: Uri, path: String, bytes: ByteArray): JSObject {
        val parts = path.split('/')
        var parentId = DocumentsContract.getTreeDocumentId(root)
        for (component in parts.dropLast(1)) {
            val matches = queryChildren(root, parentId).filter { it.name == component }
            parentId = when {
                matches.size > 1 -> throw UnknownWriteException()
                matches.size == 1 && matches.single().mime == DocumentsContract.Document.MIME_TYPE_DIR -> matches.single().documentId
                matches.size == 1 -> throw ExistingException()
                else -> {
                    val parentUri = DocumentsContract.buildDocumentUriUsingTree(root, parentId)
                    val created = DocumentsContract.createDocument(
                        resolver,
                        parentUri,
                        DocumentsContract.Document.MIME_TYPE_DIR,
                        component,
                    ) ?: throw UnknownWriteException()
                    DocumentsContract.getDocumentId(created)
                }
            }
        }
        val name = parts.last()
        if (queryChildren(root, parentId).any { it.name == name }) throw ExistingException()
        val parentUri = DocumentsContract.buildDocumentUriUsingTree(root, parentId)
        val created = DocumentsContract.createDocument(
            resolver,
            parentUri,
            "application/octet-stream",
            name,
        ) ?: throw UnknownWriteException()
        val documentId = DocumentsContract.getDocumentId(created)
        writeAndVerify(root, documentId, bytes)
        val matches = queryChildren(root, parentId).filter { it.name == name }
        if (matches.size != 1 || matches.single().documentId != documentId) throw UnknownWriteException()
        return savedBinaryResponse(bytes)
    }

    private fun writeAndVerify(root: Uri, documentId: String, bytes: ByteArray) {
        val uri = DocumentsContract.buildDocumentUriUsingTree(root, documentId)
        resolver.openFileDescriptor(uri, "rwt")?.use { descriptor ->
            FileOutputStream(descriptor.fileDescriptor).use { output ->
                output.write(bytes)
                output.flush()
                descriptor.fileDescriptor.sync()
            }
        } ?: throw MissingException()
        if (!readBounded(root, documentId).contentEquals(bytes)) throw UnknownWriteException()
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
                if (output.size() + count > MAX_TRANSFER_BYTES) throw LimitException()
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

    private fun binaryResponse(bytes: ByteArray): JSObject = JSObject().apply {
        put("bytesBase64", Base64.encodeToString(bytes, Base64.NO_WRAP))
        put("revisionHex", digest(bytes))
        put("byteLen", bytes.size.toLong())
    }

    private fun savedBinaryResponse(bytes: ByteArray): JSObject = JSObject().apply {
        put("outcome", "saved")
        put("revisionHex", digest(bytes))
        put("byteLen", bytes.size.toLong())
    }

    private fun saveOutcome(outcome: String): JSObject = JSObject().apply { put("outcome", outcome) }

    private fun canonicalNotePath(value: String): String {
        val canonical = canonicalPortablePath(value)
        if (!canonical.endsWith(".md", true)) throw IllegalArgumentException()
        return canonical
    }

    private fun canonicalPortablePath(value: String): String {
        if (value.isEmpty() || value.startsWith('/') || value.contains('\\') || value.contains('\u0000')) throw IllegalArgumentException()
        val parts = value.split('/')
        if (parts.size > MAX_DEPTH || parts.any { it.isEmpty() || it == "." || it == ".." }) throw IllegalArgumentException()
        if (isProtectedRootName(parts.first())) throw IllegalArgumentException()
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

    private fun requireRoot(expectedRootIdentityHex: String): Uri {
        if (!REVISION.matches(expectedRootIdentityHex)) throw RootMismatchException()
        val root = persistedRoot() ?: throw SecurityException("no root")
        if (rootIdentity(root) != expectedRootIdentityHex) throw RootMismatchException()
        return root
    }

    private fun activeRootResponse(root: Uri): JSObject = JSObject().apply {
        put("active", true)
        put("rootIdentityHex", rootIdentity(root))
    }

    private fun inactiveRootResponse(): JSObject = JSObject().apply {
        put("active", false)
    }

    private fun rootIdentity(root: Uri): String = stableRootIdentityHex(root.toString())

    private fun clearRootIfMatches(expectedRootIdentityHex: String) {
        synchronized(ioLock) {
            val persisted = preferences.getString(ROOT_KEY, null) ?: return@synchronized
            val currentIdentity = stableRootIdentityHex(Uri.parse(persisted).toString())
            if (currentIdentity == expectedRootIdentityHex) clearRoot()
        }
    }

    private fun clearRoot() { preferences.edit().remove(ROOT_KEY).commit() }
    private fun digest(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

    private class MissingException : Exception()
    private class LimitException : Exception()
    private class StaleException : Exception()
    private class ExistingException : Exception()
    private class UnknownWriteException : Exception()
    private class CharacterCodingException : Exception()
    private class RootMismatchException : Exception()

    companion object {
        private const val ROOT_KEY = "root-uri"
        private const val MAX_NOTE_BYTES = 16 * 1024 * 1024
        private const val MAX_TRANSFER_BYTES = 16 * 1024 * 1024
        private const val MAX_ENTRIES = 5000
        private const val MAX_DEPTH = 64
        private val REVISION = Regex("^[0-9a-f]{64}$")
    }
}

private const val ROOT_IDENTITY_DOMAIN = "myvault:saf-root:v1\u0000"
