# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile
# The display helper is not an Activity or a Service Kotlin references: it is
# started by name (`app_process ... net.syscity.desktop.helper.HelperMain`), so
# R8 cannot see the entry point and would rename or drop it in a release build.
-keep class net.syscity.desktop.helper.** { *; }

# JNI entry points. Defensive, not load-bearing: the default rules already keep
# classes with native methods
# (`-keepclasseswithmembernames,includedescriptorclasses class * { native <methods>; }`),
# and the generated `Rust` class demonstrably keeps its name in a release build.
# This exists so `NativeEvents` does not depend on that default rule surviving an
# AGP upgrade. Note the helper rule above is *not* precedent for this one: that
# one exists because `app_process` starts the class by name.
-keep class net.syscity.desktop.NativeEvents { *; }
