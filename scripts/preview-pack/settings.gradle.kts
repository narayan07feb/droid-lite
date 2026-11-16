// Builds Droid Lite's preview base pack (scripts/make-preview-pack.sh). Gradle runs here once, to
// fetch the libraries; previews themselves never run Gradle.
pluginManagement { repositories { google(); mavenCentral(); gradlePluginPortal() } }
dependencyResolutionManagement { repositories { google(); mavenCentral() } }
rootProject.name = "droid-lite-preview-pack"
include(":app", ":worker")
