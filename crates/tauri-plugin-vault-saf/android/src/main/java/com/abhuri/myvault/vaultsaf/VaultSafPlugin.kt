package com.abhuri.myvault.vaultsaf

import android.app.Activity
import android.database.ContentObserver
import android.content.Intent
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.util.Base64
import androidx.activity.result.ActivityResult
import androidx.appcompat.app.AppCompatActivity
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
import java.io.InputStream
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.ArrayDeque
import java.util.UUID
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
internal class BinaryReadBeginArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var path: String
    lateinit var sessionId: String
}

@InvokeArg
internal class BinaryReadChunkArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var sessionId: String
    var offset: Long = -1
}

@InvokeArg
internal class BinaryWriteBeginArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var path: String
    lateinit var sha256Hex: String
    lateinit var sessionId: String
    var byteLen: Long = -1
}

@InvokeArg
internal class BinaryWriteChunkArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var sessionId: String
    lateinit var bytesBase64: String
    var offset: Long = -1
}

@InvokeArg
internal class BinarySessionArgs {
    lateinit var expectedRootIdentityHex: String
    lateinit var sessionId: String
}

private data class Child(
    val documentId: String,
    val name: String,
    val mime: String,
    val size: Long,
    val sizeKnown: Boolean,
)
private data class PendingDirectory(val documentId: String, val path: String, val depth: Int)
private data class BinaryReadSession(
    val id: String,
    val rootIdentityHex: String,
    val root: Uri,
    val path: String,
    val documentId: String,
    val input: InputStream,
    val digest: MessageDigest,
    var read: Long,
)
private data class BinaryWriteSession(
    val id: String,
    val rootIdentityHex: String,
    val root: Uri,
    val path: String,
    val documentId: String,
    val expectedSha256Hex: String,
    val expectedByteLen: Long,
    val descriptor: ParcelFileDescriptor,
    val output: FileOutputStream,
    val digest: MessageDigest,
    var written: Long,
)

internal const val BINARY_MAX_TRANSFER_BYTES = 16 * 1024 * 1024
internal const val BINARY_BRIDGE_CHUNK_BYTES = 192 * 1024

internal fun isCanonicalBinarySessionId(value: String): Boolean = try {
    val parsed = UUID.fromString(value)
    parsed.version() == 4 && parsed.toString() == value
} catch (_: IllegalArgumentException) {
    false
}

internal fun isValidBinaryReadOffset(offset: Long): Boolean =
    offset in 0..BINARY_MAX_TRANSFER_BYTES.toLong()

internal fun isBinaryChunkEof(chunkSize: Int): Boolean =
    chunkSize in 0 until BINARY_BRIDGE_CHUNK_BYTES

internal fun nextBinaryWriteOffset(
    written: Long,
    expectedByteLen: Long,
    requestOffset: Long,
    chunkSize: Int,
): Long? {
    if (chunkSize !in 1..BINARY_BRIDGE_CHUNK_BYTES || requestOffset != written) return null
    val next = written + chunkSize
    return next.takeIf { next >= written && next <= expectedByteLen }
}

internal fun malformedBinaryWriteOutcome(hasActiveWriteSession: Boolean): String =
    if (hasActiveWriteSession) "writeOutcomeUnknown" else "invalidRequest"

internal fun malformedBinaryReadOutcome(hasActiveReadSession: Boolean): String =
    if (hasActiveReadSession) "nativeBridge" else "invalidRequest"

internal enum class BinarySessionOwnership { IDLE, OWNER, FOREIGN }

internal fun classifyBinarySessionOwnership(
    activeSessionId: String?,
    activeRootIdentityHex: String?,
    requestSessionId: String?,
    requestRootIdentityHex: String?,
): BinarySessionOwnership = when {
    activeSessionId == null && activeRootIdentityHex == null -> BinarySessionOwnership.IDLE
    activeSessionId == requestSessionId && activeRootIdentityHex == requestRootIdentityHex ->
        BinarySessionOwnership.OWNER
    else -> BinarySessionOwnership.FOREIGN
}

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

internal data class SafChangeHint(val dirty: Boolean, val generation: Long)

/**
 * Coalesces any number of provider callbacks into one opaque dirty generation.
 *
 * This tracker intentionally carries no URI, document ID, display name, or
 * content evidence. A successful bounded inventory consumes only the exact
 * generation it observed, so a later callback cannot be cleared by an older
 * scan.
 */
internal class SafDirtyGeneration {
    private var dirty = false
    private var generation = 0L

    fun markDirty() {
        if (dirty) return
        generation = if (generation >= MAX_SAFE_GENERATION) 1 else generation + 1
        dirty = true
    }

    fun resetForNewRoot() {
        dirty = false
        generation = 0
        markDirty()
    }

    fun snapshot(): SafChangeHint = SafChangeHint(dirty, generation)

    fun consume(observedGeneration: Long) {
        if (dirty && observedGeneration == generation) dirty = false
    }

    companion object {
        // Keep the value exact across the JSON bridge as well as Kotlin/Rust.
        private const val MAX_SAFE_GENERATION = 9_007_199_254_740_991L
    }
}

@TauriPlugin
class VaultSafPlugin(private val activity: Activity) : Plugin(activity) {
    private val resolver = activity.applicationContext.contentResolver
    private val preferences = activity.applicationContext.getSharedPreferences("myvault-saf", 0)
    private val operationInFlight = AtomicBoolean(false)
    private val ioLock = Any()
    private val dirtyGeneration = SafDirtyGeneration()
    private var foreground = true
    private var observedRootIdentity: String? = null
    private var observerRegistered = false
    private var binaryReadSession: BinaryReadSession? = null
    private var binaryWriteSession: BinaryWriteSession? = null
    private val contentObserver = object : ContentObserver(null) {
        override fun onChange(selfChange: Boolean, uri: Uri?) {
            synchronized(ioLock) {
                // The callback URI is deliberately ignored. Registration is
                // already scoped to the held tree capability, and only an
                // opaque coalesced hint may cross the native bridge.
                if (observedRootIdentity != null) dirtyGeneration.markDirty()
            }
        }
    }

    override fun onResume() {
        synchronized(ioLock) {
            foreground = true
            val root = persistedRoot() ?: return@synchronized
            // The observer is absent while paused, so resumption itself is a
            // conservative dirty hint even when no callback was delivered.
            dirtyGeneration.markDirty()
            ensureObserver(root)
        }
    }

    override fun onPause() {
        synchronized(ioLock) {
            foreground = false
            unregisterObserver()
        }
    }

    override fun onDestroy(activity: AppCompatActivity) {
        synchronized(ioLock) {
            closeBinaryReadSession()
            closeBinaryWriteSession()
            foreground = false
            unregisterObserver()
        }
    }

    @Command
    fun status(invoke: Invoke) {
        val response = synchronized(ioLock) {
            try {
                binaryReadSession?.let { return@synchronized activeRootResponse(it.root) }
                binaryWriteSession?.let { return@synchronized activeRootResponse(it.root) }
                val root = persistedRoot() ?: return@synchronized inactiveRootResponse()
                DocumentsContract.getTreeDocumentId(root)
                queryChildren(root, DocumentsContract.getTreeDocumentId(root))
                ensureObserver(root)
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
        if (synchronized(ioLock) { activeBinarySessionId() != null }) {
            invoke.reject("A native Vault transfer is active", "PICKER_BUSY")
            return
        }
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
                requireNoBinarySession()
                val previousRoot = persistedRoot()
                // Validate that the tree root is queryable before replacing the old capability.
                DocumentsContract.getTreeDocumentId(uri)
                queryChildren(uri, DocumentsContract.getTreeDocumentId(uri))
                if (!preferences.edit().putString(ROOT_KEY, uri.toString()).commit()) {
                    throw IllegalStateException("root preference was not persisted")
                }
                activationCommitted = true
                activateObserver(uri)
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
            val response = synchronized(ioLock) {
                val root = requireRoot(expectedRootIdentity)
                ensureObserver(root)
                val hint = dirtyGeneration.snapshot()
                val inventory = buildInventory(root)
                dirtyGeneration.consume(hint.generation)
                inventory.put("changeGeneration", hint.generation)
                inventory
            }
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
    fun changeHint(invoke: Invoke) {
        val expectedRootIdentity = try { invoke.parseArgs(RootArgs::class.java).expectedRootIdentityHex }
        catch (_: Exception) {
            invoke.reject("Vault capability is invalid", "VAULT_UNAVAILABLE")
            return
        }
        try {
            val response = synchronized(ioLock) {
                val root = requireRoot(expectedRootIdentity)
                ensureObserver(root)
                changeHintResponse(dirtyGeneration.snapshot())
            }
            invoke.resolve(response)
        } catch (_: RootMismatchException) {
            invoke.reject("Vault capability is stale", "VAULT_UNAVAILABLE")
        } catch (_: SecurityException) {
            clearRootIfMatches(expectedRootIdentity)
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: Exception) {
            invoke.reject("Vault change hint is unavailable", "VAULT_UNAVAILABLE")
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
    fun beginBinaryRead(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinaryReadBeginArgs::class.java) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val path = try { canonicalPortablePath(args.path) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        if (!isCanonicalBinarySessionId(args.sessionId)) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        try {
            val response = synchronized(ioLock) {
                requireNoBinarySession()
                val root = requireRoot(args.expectedRootIdentityHex)
                val document = resolveDocument(root, path) ?: throw MissingException()
                if (document.mime == DocumentsContract.Document.MIME_TYPE_DIR) throw MissingException()
                val uri = DocumentsContract.buildDocumentUriUsingTree(root, document.documentId)
                val input = resolver.openInputStream(uri) ?: throw MissingException()
                try {
                    binaryReadSession = BinaryReadSession(
                        args.sessionId,
                        args.expectedRootIdentityHex,
                        root,
                        path,
                        document.documentId,
                        input,
                        MessageDigest.getInstance("SHA-256"),
                        0,
                    )
                    saveOutcome("ready").apply { put("sessionId", args.sessionId) }
                } catch (error: Exception) {
                    try { input.close() } catch (_: Exception) {}
                    throw error
                }
            }
            invoke.resolve(response)
        } catch (_: BinarySessionBusyException) {
            invoke.resolve(saveOutcome("busy"))
        } catch (_: RootMismatchException) {
            invoke.resolve(saveOutcome("vaultUnavailable"))
        } catch (_: MissingException) {
            invoke.resolve(saveOutcome("notFound"))
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.resolve(saveOutcome("vaultUnavailable"))
        } catch (_: Exception) {
            invoke.resolve(saveOutcome("nativeBridge"))
        }
    }

    @Command
    fun readBinaryChunk(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinaryReadChunkArgs::class.java) }
        catch (_: Exception) {
            val hadActiveSession = synchronized(ioLock) { binaryReadSession != null }
            if (hadActiveSession) {
                invoke.reject("Binary read transcript is malformed", "NATIVE_BRIDGE")
            } else {
                invoke.reject("Invalid binary read request", "INVALID_PATH")
            }
            return
        }
        if (!isCanonicalBinarySessionId(args.sessionId) || !isValidBinaryReadOffset(args.offset)) {
            val ownership = synchronized(ioLock) {
                val session = binaryReadSession
                classifyBinarySessionOwnership(
                    session?.id,
                    session?.rootIdentityHex,
                    args.sessionId,
                    args.expectedRootIdentityHex,
                ).also {
                    if (it == BinarySessionOwnership.OWNER) {
                        closeBinaryReadSession(args.sessionId)
                    }
                }
            }
            if (ownership != BinarySessionOwnership.IDLE) {
                invoke.reject("Binary read transcript is malformed", "NATIVE_BRIDGE")
            } else {
                invoke.reject("Transfer object exceeds the safety limit", "RESOURCE_LIMIT")
            }
            return
        }
        try {
            val response = synchronized(ioLock) {
                val session = binaryReadSession ?: throw BinarySessionMismatchException()
                if (classifyBinarySessionOwnership(
                        session.id,
                        session.rootIdentityHex,
                        args.sessionId,
                        args.expectedRootIdentityHex,
                    ) != BinarySessionOwnership.OWNER) {
                    throw BinarySessionMismatchException()
                }
                if (session.read != args.offset) {
                    closeBinaryReadSession(args.sessionId)
                    throw BinarySessionMismatchException()
                }
                requireRoot(args.expectedRootIdentityHex, args.sessionId)
                val chunk = readNextChunk(session.input)
                val next = session.read + chunk.size
                if (next > BINARY_MAX_TRANSFER_BYTES) {
                    closeBinaryReadSession(args.sessionId)
                    throw LimitException()
                }
                session.digest.update(chunk)
                session.read = next
                val eof = isBinaryChunkEof(chunk.size)
                JSObject().apply {
                    put("offset", args.offset)
                    put("bytesBase64", Base64.encodeToString(chunk, Base64.NO_WRAP))
                    put("eof", eof)
                    if (eof) {
                        val streamedDigest = session.digest.digest().toHex()
                        val streamedLength = session.read
                        closeBinaryReadSession(args.sessionId)
                        val current = resolveDocument(session.root, session.path)
                            ?.takeIf {
                                it.documentId == session.documentId &&
                                    it.mime != DocumentsContract.Document.MIME_TYPE_DIR
                            }
                            ?: throw StaleException()
                        val verification = verifyBinaryDocument(session.root, current.documentId)
                        if (verification.first != streamedDigest || verification.second != streamedLength) {
                            throw StaleException()
                        }
                        put("revisionHex", streamedDigest)
                        put("byteLen", streamedLength)
                    }
                }
            }
            invoke.resolve(response)
        } catch (_: BinarySessionMismatchException) {
            invoke.reject("Binary read transcript is stale", "NATIVE_BRIDGE")
        } catch (_: RootMismatchException) {
            closeBinaryReadSession(args.sessionId)
            invoke.reject("Vault capability is stale", "VAULT_UNAVAILABLE")
        } catch (_: StaleException) {
            closeBinaryReadSession(args.sessionId)
            invoke.reject("Transfer object changed during the native read", "NATIVE_BRIDGE")
        } catch (_: LimitException) {
            closeBinaryReadSession(args.sessionId)
            invoke.reject("Transfer object exceeds the safety limit", "RESOURCE_LIMIT")
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.reject("Vault permission is unavailable", "VAULT_UNAVAILABLE")
        } catch (_: Exception) {
            closeBinaryReadSession(args.sessionId)
            invoke.reject("Transfer object is unavailable", "VAULT_UNAVAILABLE")
        }
    }

    @Command
    fun abortBinaryRead(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinarySessionArgs::class.java) }
        catch (_: Exception) {
            val hadActiveSession = synchronized(ioLock) { binaryReadSession != null }
            invoke.resolve(saveOutcome(malformedBinaryReadOutcome(hadActiveSession)))
            return
        }
        val response = synchronized(ioLock) {
            val session = binaryReadSession
            val ownership = classifyBinarySessionOwnership(
                session?.id,
                session?.rootIdentityHex,
                args.sessionId,
                args.expectedRootIdentityHex,
            )
            if (ownership != BinarySessionOwnership.OWNER) {
                return@synchronized saveOutcome(
                    malformedBinaryReadOutcome(ownership == BinarySessionOwnership.FOREIGN),
                )
            }
            closeBinaryReadSession(args.sessionId)
            saveOutcome("aborted")
        }
        invoke.resolve(response)
    }

    @Command
    fun beginBinaryCreate(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinaryWriteBeginArgs::class.java) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        val path = try { canonicalPortablePath(args.path) }
        catch (_: Exception) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        if (!REVISION.matches(args.sha256Hex) || !isCanonicalBinarySessionId(args.sessionId) ||
            args.byteLen < 0 || args.byteLen > BINARY_MAX_TRANSFER_BYTES) {
            invoke.resolve(saveOutcome("invalidRequest"))
            return
        }
        try {
            val response = synchronized(ioLock) {
                requireNoBinarySession()
                val root = requireRoot(args.expectedRootIdentityHex)
                val documentId = createEmptyBinaryDocument(root, path)
                val uri = DocumentsContract.buildDocumentUriUsingTree(root, documentId)
                val descriptor = try {
                    resolver.openFileDescriptor(uri, "rwt") ?: throw UnknownWriteException()
                } catch (error: Exception) {
                    throw UnknownWriteException(error)
                }
                try {
                    val output = FileOutputStream(descriptor.fileDescriptor)
                    binaryWriteSession = BinaryWriteSession(
                        args.sessionId,
                        args.expectedRootIdentityHex,
                        root,
                        path,
                        documentId,
                        args.sha256Hex,
                        args.byteLen,
                        descriptor,
                        output,
                        MessageDigest.getInstance("SHA-256"),
                        0,
                    )
                    saveOutcome("ready").apply { put("sessionId", args.sessionId) }
                } catch (error: Exception) {
                    try { descriptor.close() } catch (_: Exception) {}
                    throw UnknownWriteException(error)
                }
            }
            invoke.resolve(response)
        } catch (_: BinarySessionBusyException) {
            invoke.resolve(saveOutcome("busy"))
        } catch (_: RootMismatchException) {
            invoke.resolve(saveOutcome("vaultUnavailable"))
        } catch (_: ExistingException) {
            invoke.resolve(saveOutcome("alreadyExists"))
        } catch (_: UnknownWriteException) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: SecurityException) {
            clearRootIfMatches(args.expectedRootIdentityHex)
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        } catch (_: Exception) {
            invoke.resolve(saveOutcome("writeOutcomeUnknown"))
        }
    }

    @Command
    fun appendBinaryChunk(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinaryWriteChunkArgs::class.java) }
        catch (_: Exception) {
            val hadActiveSession = synchronized(ioLock) { binaryWriteSession != null }
            invoke.resolve(saveOutcome(malformedBinaryWriteOutcome(hadActiveSession)))
            return
        }
        val chunk = try { Base64.decode(args.bytesBase64, Base64.NO_WRAP) }
        catch (_: Exception) {
            val ownership = synchronized(ioLock) {
                val session = binaryWriteSession
                classifyBinarySessionOwnership(
                    session?.id,
                    session?.rootIdentityHex,
                    args.sessionId,
                    args.expectedRootIdentityHex,
                ).also {
                    if (it == BinarySessionOwnership.OWNER) {
                        closeBinaryWriteSession(args.sessionId)
                    }
                }
            }
            invoke.resolve(
                saveOutcome(
                    malformedBinaryWriteOutcome(ownership != BinarySessionOwnership.IDLE),
                ),
            )
            return
        }
        val response = synchronized(ioLock) {
            val session = binaryWriteSession
                ?: return@synchronized saveOutcome(malformedBinaryWriteOutcome(false))
            val ownership = classifyBinarySessionOwnership(
                session.id,
                session.rootIdentityHex,
                args.sessionId,
                args.expectedRootIdentityHex,
            )
            if (ownership != BinarySessionOwnership.OWNER) {
                return@synchronized saveOutcome(
                    malformedBinaryWriteOutcome(ownership == BinarySessionOwnership.FOREIGN),
                )
            }
            val nextOffset = nextBinaryWriteOffset(
                session.written,
                session.expectedByteLen,
                args.offset,
                chunk.size,
            )
            if (nextOffset == null) {
                closeBinaryWriteSession(args.sessionId)
                return@synchronized saveOutcome("writeOutcomeUnknown")
            }
            try {
                requireRoot(args.expectedRootIdentityHex, args.sessionId)
                session.output.write(chunk)
                session.digest.update(chunk)
                session.written = nextOffset
                saveOutcome("accepted").apply { put("nextOffset", session.written) }
            } catch (_: Exception) {
                closeBinaryWriteSession(args.sessionId)
                saveOutcome("writeOutcomeUnknown")
            }
        }
        invoke.resolve(response)
    }

    @Command
    fun finishBinaryCreate(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinarySessionArgs::class.java) }
        catch (_: Exception) {
            val hadActiveSession = synchronized(ioLock) { binaryWriteSession != null }
            invoke.resolve(saveOutcome(malformedBinaryWriteOutcome(hadActiveSession)))
            return
        }
        val response = synchronized(ioLock) {
            val session = binaryWriteSession
                ?: return@synchronized saveOutcome(malformedBinaryWriteOutcome(false))
            val ownership = classifyBinarySessionOwnership(
                session.id,
                session.rootIdentityHex,
                args.sessionId,
                args.expectedRootIdentityHex,
            )
            if (ownership != BinarySessionOwnership.OWNER) {
                return@synchronized saveOutcome(
                    malformedBinaryWriteOutcome(ownership == BinarySessionOwnership.FOREIGN),
                )
            }
            try {
                requireRoot(args.expectedRootIdentityHex, args.sessionId)
                if (session.written != session.expectedByteLen ||
                    session.digest.digest().toHex() != session.expectedSha256Hex) {
                    closeBinaryWriteSession(args.sessionId)
                    return@synchronized saveOutcome("writeOutcomeUnknown")
                }
                session.output.flush()
                session.descriptor.fileDescriptor.sync()
                closeBinaryWriteSession(args.sessionId)
                val current = resolveDocument(session.root, session.path)
                    ?.takeIf {
                        it.documentId == session.documentId &&
                            it.mime != DocumentsContract.Document.MIME_TYPE_DIR
                    }
                    ?: return@synchronized saveOutcome("writeOutcomeUnknown")
                val verification = verifyBinaryDocument(session.root, current.documentId)
                if (verification.second != session.expectedByteLen ||
                    verification.first != session.expectedSha256Hex) {
                    saveOutcome("writeOutcomeUnknown")
                } else {
                    savedBinaryEvidence(verification.first, verification.second)
                }
            } catch (_: Exception) {
                closeBinaryWriteSession(args.sessionId)
                saveOutcome("writeOutcomeUnknown")
            }
        }
        invoke.resolve(response)
    }

    @Command
    fun abortBinaryCreate(invoke: Invoke) {
        val args = try { invoke.parseArgs(BinarySessionArgs::class.java) }
        catch (_: Exception) {
            val hadActiveSession = synchronized(ioLock) { binaryWriteSession != null }
            invoke.resolve(saveOutcome(malformedBinaryWriteOutcome(hadActiveSession)))
            return
        }
        val response = synchronized(ioLock) {
            val session = binaryWriteSession
                ?: return@synchronized saveOutcome(malformedBinaryWriteOutcome(false))
            val ownership = classifyBinarySessionOwnership(
                session.id,
                session.rootIdentityHex,
                args.sessionId,
                args.expectedRootIdentityHex,
            )
            if (ownership != BinarySessionOwnership.OWNER) {
                return@synchronized saveOutcome(
                    malformedBinaryWriteOutcome(ownership == BinarySessionOwnership.FOREIGN),
                )
            }
            closeBinaryWriteSession(args.sessionId)
            saveOutcome("aborted")
        }
        invoke.resolve(response)
    }

    @Command
    fun replaceBinary(invoke: Invoke) {
        // SAF exposes neither atomic compare-and-swap nor a portable
        // evacuation/no-replace sequence. R2 therefore never mutates an
        // existing transfer target; R3 conflict handling owns that decision.
        invoke.resolve(saveOutcome("unsupportedReplace"))
    }

    private fun createEmptyBinaryDocument(root: Uri, path: String): String {
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
        val matches = queryChildren(root, parentId).filter { it.name == name }
        if (matches.size != 1 || matches.single().documentId != documentId) throw UnknownWriteException()
        return documentId
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
                    entry.put("byteLenKnown", child.sizeKnown)
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
                val sizeKnown = !cursor.isNull(3)
                children.add(
                    Child(
                        cursor.getString(0),
                        cursor.getString(1) ?: "",
                        cursor.getString(2) ?: "",
                        if (sizeKnown) cursor.getLong(3) else 0,
                        sizeKnown,
                    ),
                )
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
                if (count == 0) throw MissingException()
                if (output.size() + count > MAX_NOTE_BYTES) throw LimitException()
                output.write(buffer, 0, count)
            }
            return output.toByteArray()
        }
        throw MissingException()
    }

    private fun readNextChunk(input: InputStream): ByteArray {
        val output = ByteArrayOutputStream(BINARY_BRIDGE_CHUNK_BYTES)
        val buffer = ByteArray(8192)
        while (output.size() < BINARY_BRIDGE_CHUNK_BYTES) {
            val count = input.read(
                buffer,
                0,
                minOf(buffer.size, BINARY_BRIDGE_CHUNK_BYTES - output.size()),
            )
            if (count < 0) break
            if (count == 0) throw MissingException()
            output.write(buffer, 0, count)
        }
        return output.toByteArray()
    }

    private fun verifyBinaryDocument(root: Uri, documentId: String): Pair<String, Long> {
        val uri = DocumentsContract.buildDocumentUriUsingTree(root, documentId)
        resolver.openInputStream(uri)?.use { input ->
            val digest = MessageDigest.getInstance("SHA-256")
            val buffer = ByteArray(8192)
            var byteLen = 0L
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                if (count == 0) throw MissingException()
                byteLen += count
                if (byteLen > BINARY_MAX_TRANSFER_BYTES) throw LimitException()
                digest.update(buffer, 0, count)
            }
            return Pair(digest.digest().toHex(), byteLen)
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

    private fun savedBinaryEvidence(revisionHex: String, byteLen: Long): JSObject = JSObject().apply {
        put("outcome", "saved")
        put("revisionHex", revisionHex)
        put("byteLen", byteLen)
    }

    private fun saveOutcome(outcome: String): JSObject = JSObject().apply { put("outcome", outcome) }

    private fun changeHintResponse(hint: SafChangeHint): JSObject = JSObject().apply {
        put("dirty", hint.dirty)
        put("generation", hint.generation)
    }

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

    private fun activateObserver(root: Uri) {
        unregisterObserver()
        dirtyGeneration.resetForNewRoot()
        ensureObserver(root)
    }

    private fun ensureObserver(root: Uri) {
        if (!foreground) return
        val identity = rootIdentity(root)
        if (observerRegistered && observedRootIdentity == identity) return
        unregisterObserver()
        observedRootIdentity = identity
        try {
            resolver.registerContentObserver(root, true, contentObserver)
            observerRegistered = true
        } catch (_: Exception) {
            // Manual/startup inventory remains authoritative. Leave the dirty
            // latch set and retry registration on the next native operation.
            observedRootIdentity = null
            observerRegistered = false
        }
    }

    private fun unregisterObserver() {
        observedRootIdentity = null
        if (!observerRegistered) return
        try {
            resolver.unregisterContentObserver(contentObserver)
        } catch (_: Exception) {
            // The identity guard above makes any queued callback inert.
        } finally {
            observerRegistered = false
        }
    }

    private fun persistedRoot(): Uri? {
        val value = preferences.getString(ROOT_KEY, null) ?: return null
        val uri = try { Uri.parse(value) } catch (_: Exception) { clearRoot(); return null }
        if (!hasPersistedReadWritePermission(uri)) { clearRoot(); return null }
        return uri
    }

    private fun activeBinarySessionId(): String? = binaryReadSession?.id ?: binaryWriteSession?.id

    private fun requireNoBinarySession() {
        if (activeBinarySessionId() != null || operationInFlight.get()) {
            throw BinarySessionBusyException()
        }
    }

    private fun requireRoot(expectedRootIdentityHex: String, ownerSessionId: String? = null): Uri {
        val activeSessionId = activeBinarySessionId()
        if (activeSessionId != null && activeSessionId != ownerSessionId) {
            throw BinarySessionBusyException()
        }
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

    private fun clearRoot() {
        closeBinaryReadSession()
        closeBinaryWriteSession()
        unregisterObserver()
        preferences.edit().remove(ROOT_KEY).commit()
    }

    private fun closeBinaryReadSession(expectedSessionId: String? = null) {
        synchronized(ioLock) {
            val session = binaryReadSession ?: return@synchronized
            if (expectedSessionId != null && session.id != expectedSessionId) return@synchronized
            binaryReadSession = null
            try { session.input.close() } catch (_: Exception) {}
        }
    }

    private fun closeBinaryWriteSession(expectedSessionId: String? = null) {
        synchronized(ioLock) {
            val session = binaryWriteSession ?: return@synchronized
            if (expectedSessionId != null && session.id != expectedSessionId) return@synchronized
            binaryWriteSession = null
            try { session.output.close() } catch (_: Exception) {}
            try { session.descriptor.close() } catch (_: Exception) {}
        }
    }
    private fun digest(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).toHex()
    private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }

    private class MissingException : Exception()
    private class LimitException : Exception()
    private class StaleException : Exception()
    private class ExistingException : Exception()
    private class UnknownWriteException(cause: Throwable? = null) : Exception(cause)
    private class CharacterCodingException : Exception()
    private class RootMismatchException : Exception()
    private class BinarySessionBusyException : Exception()
    private class BinarySessionMismatchException : Exception()

    companion object {
        private const val ROOT_KEY = "root-uri"
        private const val MAX_NOTE_BYTES = 16 * 1024 * 1024
        private const val MAX_ENTRIES = 5000
        private const val MAX_DEPTH = 64
        private val REVISION = Regex("^[0-9a-f]{64}$")
    }
}

private const val ROOT_IDENTITY_DOMAIN = "myvault:saf-root:v1\u0000"
