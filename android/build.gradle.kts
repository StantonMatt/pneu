buildscript {
    dependencies {
        // Upgrade the Kotlin compiler used by AGP's built-in Kotlin support.
        classpath(libs.kotlin.gradle.plugin)
    }
}

plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.compose) apply false
}
