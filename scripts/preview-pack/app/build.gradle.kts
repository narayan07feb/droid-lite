// Compose for projects that bring none, and what such a file compiles against.
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}
android {
    namespace = "dev.droidlite.pack"
    compileSdk = 35
    defaultConfig { minSdk = 24 }
}
dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.01.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.activity:activity-compose:1.9.3")
}
val artifactType = Attribute.of("artifactType", String::class.java)
fun classes(conf: String) = configurations.getByName(conf).incoming
    .artifactView { attributes { attribute(artifactType, "android-classes-jar") } }.files
// lazy: AGP creates the variant configurations after this script runs
val runtimeJars = files({ classes("debugRuntimeClasspath") })
val compileJars = files({ classes("debugCompileClasspath") })
tasks.register("packLists") {
    dependsOn("processDebugResources")
    inputs.files(runtimeJars, compileJars)
    val out = layout.buildDirectory.dir("pack").get().asFile
    doLast {
        out.mkdirs()
        File(out, "compose.txt").writeText(runtimeJars.files.joinToString("\n"))
        File(out, "compile.txt").writeText(compileJars.files.joinToString("\n"))
    }
}
