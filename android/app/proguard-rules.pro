# R8 rules for the release build.
#
# The app is small and uses no reflection of its own, so the defaults from
# proguard-android-optimize.txt cover almost everything. The entries below are
# the cases R8 cannot infer.

# NotificationListenerService is instantiated by the system from the manifest
# entry, so nothing in our code references the constructor. Without this it
# would be shrunk away and auto-reply would silently stop working.
# (business edition only; the client build has no such class — harmless there.)
-keep class tech.yaya.agente.AgenteNotificationListener { *; }
-dontwarn tech.yaya.agente.AgenteNotificationListener
-keep class tech.yaya.agente.DownloadReceiver { *; }
# JNI entry points of the on-device agent core (libagente_core.so) are resolved
# by name at runtime.
-keep class tech.yaya.agente.AgenteCore { *; }
-keepclasseswithmembernames class * { native <methods>; }

# Keystore-backed crypto is reached through JCA provider names, not direct
# references.
-keep class javax.crypto.** { *; }

# Keep line numbers so a stack trace from a release build is still readable,
# while dropping the source file names themselves.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
