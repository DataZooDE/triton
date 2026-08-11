plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    namespace = "de.datazoo.triton_explorer"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "de.datazoo.triton_explorer"
        minSdk = flutter.minSdkVersion
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName

        // `oidc_android` pulls in flutter_appauth, whose manifest declares a
        // redirect intent-filter with an `${appAuthRedirectScheme}`
        // placeholder; without a value here the manifest merger fails the
        // build outright ("requires a placeholder substitution").
        //
        // The Explorer's OIDC flow is web-only (`auth_manager.dart` returns no
        // manager unless `kIsWeb`), so nothing ever redirects to this scheme —
        // it exists to let the merger resolve. Give it the application id so
        // the registered scheme is at least unambiguously ours, and revisit it
        // the day native sign-in is actually wired up.
        manifestPlaceholders["appAuthRedirectScheme"] = "de.datazoo.triton_explorer"
    }

    buildTypes {
        release {
            // TODO: Add your own signing config for the release build.
            // Signing with the debug keys for now, so `flutter run --release` works.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}
