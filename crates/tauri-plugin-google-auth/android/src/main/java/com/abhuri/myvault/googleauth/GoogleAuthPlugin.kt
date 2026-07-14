package com.abhuri.myvault.googleauth

import android.accounts.Account
import android.app.Activity
import android.os.Handler
import android.os.Looper
import androidx.activity.result.ActivityResult
import androidx.activity.result.IntentSenderRequest
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.google.android.gms.auth.api.identity.AuthorizationRequest
import com.google.android.gms.auth.api.identity.AuthorizationResult
import com.google.android.gms.auth.api.identity.ClearTokenRequest
import com.google.android.gms.auth.api.identity.Identity
import com.google.android.gms.auth.api.identity.RevokeAccessRequest
import com.google.android.gms.common.ConnectionResult
import com.google.android.gms.common.GoogleApiAvailability
import com.google.android.gms.common.api.Scope
import java.util.concurrent.atomic.AtomicBoolean

@InvokeArg
internal class AuthorizeArgs {
    var scopes: List<String> = emptyList()
}

@InvokeArg
internal class DisconnectArgs {
    lateinit var accessToken: String
}

@TauriPlugin
class GoogleAuthPlugin(private val activity: Activity) : Plugin(activity) {
    private val allowedScope = "https://www.googleapis.com/auth/drive"
    private val authorizationClient = Identity.getAuthorizationClient(activity)
    private val operationInFlight = AtomicBoolean(false)
    private val mainHandler = Handler(Looper.getMainLooper())
    private var timeoutTask: Runnable? = null
    private var authorizedAccount: Account? = null
    private var authorizedScopes: List<Scope> = emptyList()
    private var requestedScopes: Set<String> = emptySet()

    @Command
    fun authorize(invoke: Invoke) {
        if (GoogleApiAvailability.getInstance().isGooglePlayServicesAvailable(activity) != ConnectionResult.SUCCESS) {
            invoke.reject("Google Play Services is unavailable", "PLAY_SERVICES_UNAVAILABLE")
            return
        }
        if (!operationInFlight.compareAndSet(false, true)) {
            invoke.reject("Another authorization operation is active", "AUTH_BUSY")
            return
        }
        startTimeout(invoke)

        val args = try {
            invoke.parseArgs(AuthorizeArgs::class.java)
        } catch (_: Exception) {
            reject(invoke, "AUTH_INVALID_REQUEST", "Invalid authorization request")
            return
        }

        if (args.scopes != listOf(allowedScope)) {
            reject(invoke, "AUTH_INVALID_REQUEST", "Only the configured full Drive scope is allowed")
            return
        }

        requestedScopes = args.scopes.toSet()

        val request = AuthorizationRequest.builder()
            .setRequestedScopes(requestedScopes.map(::Scope))
            .build()

        authorizationClient.authorize(request)
            .addOnSuccessListener { result ->
                if (result.hasResolution()) {
                    val pendingIntent = result.pendingIntent
                    if (pendingIntent == null) {
                        reject(invoke, "AUTH_UNAVAILABLE", "Authorization is unavailable")
                        return@addOnSuccessListener
                    }

                    try {
                        startIntentSenderForResult(
                            invoke,
                            IntentSenderRequest.Builder(pendingIntent).build(),
                            "authorizationResult"
                        )
                    } catch (_: Exception) {
                        reject(invoke, "AUTH_UI_FAILED", "Authorization UI could not be opened")
                    }
                } else {
                    resolveAuthorization(invoke, result)
                }
            }
            .addOnFailureListener {
                reject(invoke, "AUTH_FAILED", "Google authorization failed")
            }
    }

    @ActivityCallback
    fun authorizationResult(invoke: Invoke, result: ActivityResult) {
        if (!operationInFlight.get()) return
        if (result.resultCode == Activity.RESULT_CANCELED) {
            reject(invoke, "AUTH_CANCELLED", "Authorization was cancelled")
            return
        }
        if (result.resultCode != Activity.RESULT_OK || result.data == null) {
            reject(invoke, "AUTH_FAILED", "Google authorization failed")
            return
        }

        try {
            resolveAuthorization(
                invoke,
                authorizationClient.getAuthorizationResultFromIntent(result.data!!)
            )
        } catch (_: Exception) {
            reject(invoke, "AUTH_FAILED", "Google authorization failed")
        }
    }

    @Command
    fun disconnect(invoke: Invoke) {
        if (!operationInFlight.compareAndSet(false, true)) {
            invoke.reject("Another authorization operation is active", "AUTH_BUSY")
            return
        }
        startTimeout(invoke)

        val token = try {
            invoke.parseArgs(DisconnectArgs::class.java).accessToken
        } catch (_: Exception) {
            reject(invoke, "AUTH_INVALID_REQUEST", "Invalid disconnect request")
            return
        }
        if (token.isBlank()) {
            reject(invoke, "AUTH_INVALID_REQUEST", "Invalid disconnect request")
            return
        }

        val clearRequest = ClearTokenRequest.builder().setToken(token).build()
        authorizationClient.clearToken(clearRequest)
            .addOnSuccessListener {
                revokeAfterClear(invoke)
            }
            .addOnFailureListener {
                reject(invoke, "AUTH_DISCONNECT_FAILED", "Google disconnect failed")
            }
    }

    private fun revokeAfterClear(invoke: Invoke) {
        val account = authorizedAccount
        val scopes = authorizedScopes
        // The access token is already cleared. Local state must become
        // disconnected even when the best-effort grant revocation fails.
        authorizedAccount = null
        authorizedScopes = emptyList()
        if (account == null || scopes.isEmpty()) {
            resolveDisconnect(invoke, false)
            return
        }

        val request = RevokeAccessRequest.builder()
            .setAccount(account)
            .setScopes(scopes)
            .build()
        authorizationClient.revokeAccess(request)
            .addOnSuccessListener {
                resolveDisconnect(invoke, true)
            }
            .addOnFailureListener {
                resolveDisconnect(invoke, false)
            }
    }

    private fun resolveAuthorization(invoke: Invoke, result: AuthorizationResult) {
        if (!operationInFlight.get()) return
        val token = result.accessToken
        if (token.isNullOrBlank()) {
            reject(invoke, "AUTH_TOKEN_MISSING", "Google did not return an access token")
            return
        }

        val grantedScopeUris = result.grantedScopes.toSet()
        if (grantedScopeUris != requestedScopes) {
            reject(invoke, "AUTH_SCOPES_MISSING", "Google did not grant exactly the requested scope")
            return
        }

        authorizedAccount = result.toGoogleSignInAccount()?.account
        authorizedScopes = result.grantedScopes.map(::Scope)

        val response = JSObject()
        response.put("accessToken", token)
        val scopes = JSArray()
        result.grantedScopes.forEach(scopes::put)
        response.put("grantedScopes", scopes)
        finishOperation()
        invoke.resolve(response)
    }

    private fun resolveDisconnect(invoke: Invoke, revoked: Boolean) {
        if (!operationInFlight.get()) return
        val response = JSObject()
        response.put("revoked", revoked)
        finishOperation()
        invoke.resolve(response)
    }

    private fun reject(invoke: Invoke, code: String, message: String) {
        if (operationInFlight.compareAndSet(true, false)) {
            cancelTimeout()
            requestedScopes = emptySet()
            invoke.reject(message, code)
        }
    }

    private fun startTimeout(invoke: Invoke) {
        cancelTimeout()
        val task = Runnable {
            reject(invoke, "AUTH_TIMEOUT", "Google authorization timed out")
        }
        timeoutTask = task
        mainHandler.postDelayed(task, 120_000L)
    }

    private fun cancelTimeout() {
        timeoutTask?.let(mainHandler::removeCallbacks)
        timeoutTask = null
    }

    private fun finishOperation() {
        cancelTimeout()
        requestedScopes = emptySet()
        operationInFlight.set(false)
    }
}
