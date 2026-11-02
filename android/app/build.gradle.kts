import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

val releasePropertiesPath = providers.gradleProperty("PNEU_RELEASE_KEYSTORE_PROPERTIES")
    .orElse(providers.environmentVariable("PNEU_RELEASE_KEYSTORE_PROPERTIES"))
    .getOrElse("${System.getProperty("user.home")}/.local/share/android-release-keys/pneu/keystore.properties")
val releasePropertiesFile = file(if (releasePropertiesPath.startsWith("~/"))
    "${System.getProperty("user.home")}/${releasePropertiesPath.removePrefix("~/")}" else releasePropertiesPath)
val releaseProperties = Properties().apply {
    if (releasePropertiesFile.isFile) releasePropertiesFile.inputStream().use { load(it) }
}
val releaseStoreFile = releaseProperties.getProperty("storeFile")?.let {
    val path = if (it.startsWith("~/")) "${System.getProperty("user.home")}/${it.removePrefix("~/")}" else it
    releasePropertiesFile.parentFile.resolve(path)
}
val validateReleaseKeystore by tasks.registering {
    doLast {
        check(releasePropertiesFile.isFile) {
            "Release signing requires $releasePropertiesFile. Set Gradle property or environment variable PNEU_RELEASE_KEYSTORE_PROPERTIES to an external keystore.properties file."
        }
        check(listOf("storeFile", "storePassword", "keyAlias", "keyPassword").all {
            !releaseProperties.getProperty(it).isNullOrBlank()
        }) { "Release keystore.properties must contain storeFile, storePassword, keyAlias and keyPassword." }
        check(releaseStoreFile?.isFile == true) { "Release signing storeFile must point to an existing upload keystore." }
    }
}
tasks.matching { it.name == "validateSigningRelease" }.configureEach { dependsOn(validateReleaseKeystore) }

android {
    namespace = "io.github.stantonmatt.pneu"
    compileSdk {
        version = release(37) {
            minorApiLevel = 2
        }
    }
    buildToolsVersion = "37.0.0"

    defaultConfig {
        applicationId = "io.github.stantonmatt.pneu"
        minSdk = 34
        targetSdk {
            version = release(37)
        }
        versionCode = 7
        versionName = "2.2.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    signingConfigs {
        create("release") {
            storeFile = releaseStoreFile
            storePassword = releaseProperties.getProperty("storePassword")
            keyAlias = releaseProperties.getProperty("keyAlias")
            keyPassword = releaseProperties.getProperty("keyPassword")
        }
    }
    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = false
        }
    }

    testOptions.unitTests.isIncludeAndroidResources = true

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
}

kotlin {
    jvmToolchain(21)
}

val protocolVectors by tasks.registering(Copy::class) {
    from(rootProject.file(providers.gradleProperty("protocolVectorsPath").getOrElse("../docs/protocol-vectors/v1.json")))
    into(layout.buildDirectory.dir("generated/protocolTestResources"))
}
android.sourceSets.getByName("test").resources.srcDir(layout.buildDirectory.dir("generated/protocolTestResources").get().asFile)
tasks.matching { it.name.contains("UnitTest") || it.name == "preBuild" }.configureEach { dependsOn(protocolVectors) }

dependencies {
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.10.0")
    implementation("androidx.datastore:datastore-preferences:1.2.1")
    implementation("androidx.room:room-runtime:2.8.4")
    implementation("androidx.room:room-ktx:2.8.4")
    annotationProcessor("androidx.room:room-compiler:2.8.4")
    implementation("com.google.code.gson:gson:2.13.2")
    implementation("androidx.camera:camera-camera2:1.5.3")
    implementation("androidx.camera:camera-lifecycle:1.5.3")
    implementation("androidx.camera:camera-view:1.5.3")
    implementation("androidx.exifinterface:exifinterface:1.4.2")
    implementation(libs.zxing.core)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.ui)
    implementation(libs.kotlinx.coroutines.android)

    testImplementation(platform(libs.androidx.compose.bom))
    testImplementation(libs.androidx.compose.ui.test.junit4)
    testImplementation(libs.junit)
    testImplementation("org.robolectric:robolectric:4.16")
    testImplementation(libs.kotlinx.coroutines.test)

    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.compose.ui.test.junit4)
    androidTestImplementation(libs.androidx.test.ext.junit)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation("androidx.test.uiautomator:uiautomator:2.3.0")
    // Compose's transitive Espresso 3.5.0 cannot initialize input on API 37.
    androidTestImplementation(libs.androidx.test.espresso.core)
    androidTestImplementation(libs.kotlinx.coroutines.test)

    debugImplementation(libs.androidx.compose.ui.tooling)
    debugImplementation(libs.androidx.compose.ui.test.manifest)
}
