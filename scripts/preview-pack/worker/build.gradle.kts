// The renderer (Paparazzi's layoutlib SDK, used only as a library by the preview worker; projects never
// apply Paparazzi), layoutlib's native runtime and framework resources, and the Compose compiler plugin
// for projects that have none in the Gradle cache.
plugins { java }
val native by configurations.creating { isTransitive = false }
val composePlugin by configurations.creating { isTransitive = false }
dependencies {
    implementation("app.cash.paparazzi:paparazzi:1.3.5")
    native("com.android.tools.layoutlib:layoutlib-runtime:14.0.11:mac-arm")
    native("com.android.tools.layoutlib:layoutlib-resources:14.0.11")
    composePlugin("org.jetbrains.kotlin:kotlin-compose-compiler-plugin-embeddable:2.4.21")
}
val out = layout.buildDirectory.dir("pack")
tasks.register<Copy>("packRuntime") { from(configurations.runtimeClasspath); into(out.map { it.dir("runtime") }) }
tasks.register<Copy>("packLayoutlibRuntime") {
    from(native.filter { "runtime" in it.name }.map { zipTree(it) })
    into(out.map { it.dir("layoutlib/runtime") })
}
tasks.register<Copy>("packLayoutlibResources") {
    from(native.filter { "resources" in it.name }.map { zipTree(it) })
    into(out.map { it.dir("layoutlib/resources") })
}
tasks.register<Copy>("packComposePlugin") { from(composePlugin); into(out); rename { "compose-plugin.jar" } }
