// Droid Lite's Compose preview worker: one warm JVM that compiles the open Kotlin file and renders its
// @Preview functions with layoutlib (Paparazzi's SDK). No Gradle. Droid Lite starts it detached on the
// first preview and talks to it over a Unix socket.
//
// Run: java -cp <base jars> PreviewWorker.java <socket path> <work dir>
//   system properties: paparazzi.layoutlib.runtime.root, paparazzi.layoutlib.resources.root,
//   droidlite.plugins (the Kotlin compiler plugin jars: Compose, parcelize; ":"-separated),
//   droidlite.compile.cp (the classpath the open file compiles against), droidlite.idle.minutes (10).
//
// Protocol, one JSON object per line each way (a tiny reader below, no JSON library):
//   -> {"id":7,"file":"/abs/Card.kt","sources":["/abs/Other.kt"],"class":"demo.CardKt","previews":["CardPreview"]}
//   <- {"id":7,"ok":true,"images":[{"name":"CardPreview","png":"/work/7-CardPreview.png","ms":48}],"compile_ms":210}
//   <- {"id":7,"ok":false,"error":"e: Card.kt:3:5 Unresolved reference 'Foo'."}
// One render at a time; the newest request wins (an older one waiting is dropped and answered "skipped").
import app.cash.paparazzi.DeviceConfig;
import app.cash.paparazzi.Environment;
import app.cash.paparazzi.PaparazziSdk;
import androidx.compose.runtime.reflect.ComposableMethod;
import androidx.compose.runtime.reflect.ComposableMethodKt;
import java.awt.image.BufferedImage;
import java.io.*;
import java.net.StandardProtocolFamily;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.Channels;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicReference;
import javax.imageio.ImageIO;

public class PreviewWorker {
    record Request(long id, String file, List<String> sources, String cls, List<String> previews, List<String> cp,
        List<String> rjava, String key, SocketChannel out) {}

    static final AtomicReference<Request> next = new AtomicReference<>();
    static final Object wake = new Object();
    static volatile long lastWork = System.currentTimeMillis();
    static BufferedImage frame;

    public static void main(String[] a) throws Exception {
        Path sock = Path.of(a[0]);
        Path work = Path.of(a[1]);
        Files.createDirectories(work);
        Files.deleteIfExists(sock);
        hostBuildFingerprint();
        long idle = Long.getLong("droidlite.idle.minutes", 10) * 60_000;
        Thread renderer = new Thread(() -> renderLoop(work), "render");
        renderer.setDaemon(true);
        renderer.start();
        Thread watchdog = new Thread(() -> {
            while (true) {
                sleep(10_000);
                if (next.get() == null && System.currentTimeMillis() - lastWork > idle) {
                    try { Files.deleteIfExists(sock); } catch (IOException ignored) {}
                    System.exit(0); // idle: give the memory back
                }
            }
        }, "idle");
        watchdog.setDaemon(true);
        watchdog.start();
        try (ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX)) {
            server.bind(UnixDomainSocketAddress.of(sock));
            System.err.println("preview worker ready on " + sock);
            while (true) {
                SocketChannel c = server.accept();
                Thread t = new Thread(() -> serve(c), "conn");
                t.setDaemon(true);
                t.start();
            }
        }
    }

    static void serve(SocketChannel c) {
        // replies go straight to the channel: Channels' stream wrappers share one lock, so a reply would wait
        // for the blocked read of the next request
        try (c; BufferedReader in = new BufferedReader(new InputStreamReader(Channels.newInputStream(c), StandardCharsets.UTF_8))) {
            SocketChannel out = c;
            String line;
            while ((line = in.readLine()) != null) {
                Map<String, Object> m = Json.parse(line);
                Request r = new Request(((Number) m.get("id")).longValue(), (String) m.get("file"),
                    strings(m.get("sources")), (String) m.get("class"), strings(m.get("previews")), strings(m.get("cp")),
                    strings(m.get("rjava")), m.get("key") == null ? "" : (String) m.get("key"), out);
                Request old = next.getAndSet(r);
                if (old != null) reply(old.out, "{\"id\":" + old.id + ",\"ok\":false,\"skipped\":true}");
                synchronized (wake) { wake.notifyAll(); }
            }
        } catch (Exception e) {
            System.err.println("connection: " + e);
        }
    }

    static void renderLoop(Path work) {
        PaparazziSdk sdk = null;
        while (true) {
            Request r;
            synchronized (wake) {
                while ((r = next.getAndSet(null)) == null) {
                    try { wake.wait(); } catch (InterruptedException e) { return; }
                }
            }
            lastWork = System.currentTimeMillis();
            try {
                if (sdk == null) {
                    status(r, "Starting the renderer", 0, 0);
                    // the libraries' assets folders (fonts and Compose resources load from them)
                    List<String> assets = list("droidlite.assets");
                    // the R packages (their R classes are on this classpath) and resource folders, so
                    // R.string.x and painterResource(R.drawable.y) resolve; the first package is the "app"
                    List<String> packages = list("droidlite.res.packages");
                    Environment env = new Environment(work.toString(), packages.isEmpty() ? "droidlite.preview" : packages.get(0),
                        34, // layoutlib 14 is Android 14 (API 34): Compose must not take its API 35 paths
                        packages.size() > 1 ? packages.subList(1, packages.size()) : List.of(), List.of(),
                        list("droidlite.res.modules"), list("droidlite.aar.dirs"), List.of(), assets);
                    // SHRINK: the picture is as big as the content (like Android Studio's preview), not a whole phone
                    sdk = new PaparazziSdk(env, DeviceConfig.PIXEL_5, "android:Theme.Material.NoActionBar.Fullscreen",
                        com.android.ide.common.rendering.api.SessionParams.RenderingMode.SHRINK,
                        img -> { frame = img; return kotlin.Unit.INSTANCE; });
                    sdk.setup();
                }
                reply(r.out, handle(r, work, sdk));
            } catch (Throwable e) {
                reply(r.out, "{\"id\":" + r.id + ",\"ok\":false,\"error\":" + Json.str(trace(e)) + "}");
            }
            lastWork = System.currentTimeMillis();
        }
    }

    /** The project's own code a preview needs (its R classes, then its source files), compiled once per
     *  key into work/lib-KEY and reused until a file changes; older lib dirs are deleted. Null when none. */
    static Path ownCode(Request r, Path work, String cp) throws Exception {
        if (r.key.isEmpty() || (r.sources.isEmpty() && r.rjava.isEmpty())) return null;
        Path lib = work.resolve("lib-" + r.key);
        if (Files.exists(lib.resolve(".ok"))) return lib;
        status(r, "Compiling the project code this preview needs (" + r.sources.size() + " files)", 0, 0);
        deleteTree(lib);
        Files.createDirectories(lib);
        if (!r.rjava.isEmpty()) javac(r.rjava, System.getProperty("droidlite.compile.cp"), lib);
        List<String> kt = r.sources.stream().filter(s -> s.endsWith(".kt")).toList();
        List<String> java = r.sources.stream().filter(s -> s.endsWith(".java")).toList();
        if (!kt.isEmpty()) {
            List<String> args = new ArrayList<>(r.sources); // .java files too: Kotlin code may use them
            args.addAll(List.of("-d", lib.toString(), "-classpath", cp + ":" + lib, "-no-stdlib", "-no-reflect",
                "-jvm-target", "17", "-nowarn", "-module-name", "project", "-Xskip-metadata-version-check",
                "-Xskip-prerelease-check"));
            args.addAll(plugins());
            ByteArrayOutputStream log = new ByteArrayOutputStream();
            var code = new org.jetbrains.kotlin.cli.jvm.K2JVMCompiler().exec(new PrintStream(log, true, StandardCharsets.UTF_8), args.toArray(String[]::new));
            if (code != org.jetbrains.kotlin.cli.common.ExitCode.OK) {
                throw new IllegalStateException(log.toString(StandardCharsets.UTF_8).strip());
            }
        }
        if (!java.isEmpty()) javac(java, cp + ":" + lib, lib);
        Files.writeString(lib.resolve(".ok"), r.key);
        try (var dirs = Files.list(work)) { // keep only the current project code
            dirs.filter(d -> d.getFileName().toString().startsWith("lib-") && !d.equals(lib)).forEach(PreviewWorker::deleteTree);
        }
        return lib;
    }

    /** -Xplugin=… for each compiler plugin of the project's Kotlin (Compose, parcelize). */
    static List<String> plugins() {
        List<String> out = new ArrayList<>();
        for (String p : System.getProperty("droidlite.plugins", "").split(":")) if (!p.isEmpty()) out.add("-Xplugin=" + p);
        return out;
    }

    static void javac(List<String> files, String cp, Path out) {
        var javac = javax.tools.ToolProvider.getSystemJavaCompiler();
        ByteArrayOutputStream log = new ByteArrayOutputStream();
        List<String> args = new ArrayList<>(List.of("-d", out.toString(), "-cp", cp, "-nowarn", "-proc:none", "--release", "17"));
        args.addAll(files);
        if (javac.run(null, log, log, args.toArray(String[]::new)) != 0) {
            throw new IllegalStateException(log.toString(StandardCharsets.UTF_8).strip());
        }
    }

    /** What Android Studio's preview gives every preview (its ComposeViewAdapter does the same):
     *  owners for activity results and the back button, so rememberLauncherForActivityResult and
     *  BackHandler work. Compiled once per worker against the project's libraries; null when the
     *  project has no activity-compose (then previews run without it). */
    static final String HOST = """
        package droidlite.host

        import androidx.activity.OnBackPressedDispatcher
        import androidx.activity.OnBackPressedDispatcherOwner
        import androidx.activity.compose.LocalActivityResultRegistryOwner
        import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
        import androidx.activity.result.ActivityResultRegistry
        import androidx.activity.result.ActivityResultRegistryOwner
        import androidx.activity.result.contract.ActivityResultContract
        import androidx.compose.runtime.Composable
        import androidx.compose.runtime.CompositionLocalProvider
        import androidx.compose.ui.platform.LocalInspectionMode
        import androidx.compose.ui.platform.LocalLifecycleOwner
        import androidx.core.app.ActivityOptionsCompat

        @Composable
        fun PreviewHost(content: @Composable () -> Unit) {
            val lifecycle = LocalLifecycleOwner.current.lifecycle
            val results = object : ActivityResultRegistryOwner {
                override val activityResultRegistry = object : ActivityResultRegistry() {
                    override fun <I, O> onLaunch(requestCode: Int, contract: ActivityResultContract<I, O>, input: I, options: ActivityOptionsCompat?) {}
                }
            }
            val back = object : OnBackPressedDispatcherOwner {
                override val lifecycle = lifecycle
                override val onBackPressedDispatcher = OnBackPressedDispatcher()
            }
            CompositionLocalProvider(
                LocalActivityResultRegistryOwner provides results,
                LocalOnBackPressedDispatcherOwner provides back,
                LocalInspectionMode provides true,
            ) {
                CMP_CONTEXT
                content()
            }
        }
        """;
    /** Compose Multiplatform resources read images and strings through an Android context that only an
     *  app start sets; previews set it with this effect (as Android Studio's preview does). */
    static final String CMP_CONTEXT = "@OptIn(org.jetbrains.compose.resources.ExperimentalResourceApi::class) org.jetbrains.compose.resources.PreviewContextConfigurationEffect()";
    static Path host;
    static boolean hostTried;

    static Path host(Path work, String cp) {
        if (hostTried) return host;
        hostTried = true;
        try {
            Path src = work.resolve("host/PreviewHost.kt");
            Path out = work.resolve("host/classes");
            Files.createDirectories(src.getParent());
            // with the Compose Multiplatform context first; a project without compose-resources gets
            // the plain host
            for (String hostSource : List.of(HOST.replace("CMP_CONTEXT", CMP_CONTEXT), HOST.replace("CMP_CONTEXT", ""))) {
                Files.writeString(src, hostSource);
                deleteTree(out);
                List<String> args = new ArrayList<>(List.of(src.toString(), "-d", out.toString(), "-classpath", cp, "-no-stdlib",
                    "-no-reflect", "-jvm-target", "17", "-nowarn", "-Xskip-metadata-version-check", "-Xskip-prerelease-check"));
                args.addAll(plugins());
                ByteArrayOutputStream log = new ByteArrayOutputStream();
                var code = new org.jetbrains.kotlin.cli.jvm.K2JVMCompiler().exec(new PrintStream(log, true, StandardCharsets.UTF_8), args.toArray(String[]::new));
                if (code == org.jetbrains.kotlin.cli.common.ExitCode.OK) {
                    host = out;
                    break;
                }
                System.err.println("preview host variant not built: " + log.toString(StandardCharsets.UTF_8).lines().limit(3).toList());
            }
        } catch (Exception e) {
            System.err.println("preview host: " + e);
        }
        return host;
    }

    static String handle(Request r, Path work, PaparazziSdk sdk) throws Exception {
        Path out = work.resolve("classes-" + r.id);
        long t0 = System.nanoTime();
        // the project's own libraries first (the worker runs with them too), then the base pack
        String cp = (r.cp.isEmpty() ? "" : String.join(":", r.cp) + ":") + System.getProperty("droidlite.compile.cp");
        Path lib;
        try {
            lib = ownCode(r, work, cp);
        } catch (IllegalStateException e) {
            return "{\"id\":" + r.id + ",\"ok\":false,\"error\":" + Json.str("In the project code this preview needs:\n" + e.getMessage()) + "}";
        }
        if (lib != null) cp = cp + ":" + lib;
        status(r, "Compiling " + Path.of(r.file).getFileName(), 0, 0);
        List<String> args = new ArrayList<>();
        args.add(r.file);
        args.addAll(List.of("-d", out.toString(), "-classpath", cp,
            // the same module as the project code, so the open file sees its `internal` declarations
            "-no-stdlib", "-no-reflect", "-jvm-target", "17", "-nowarn", "-module-name", "project",
            // the project's libraries may be built by a newer Kotlin than the base pack's compiler
            "-Xskip-metadata-version-check", "-Xskip-prerelease-check"));
        args.addAll(plugins());
        if (lib != null) args.add("-Xfriend-paths=" + lib);
        ByteArrayOutputStream log = new ByteArrayOutputStream();
        var code = new org.jetbrains.kotlin.cli.jvm.K2JVMCompiler().exec(new PrintStream(log, true, StandardCharsets.UTF_8), args.toArray(String[]::new));
        long compileMs = (System.nanoTime() - t0) / 1_000_000;
        if (code != org.jetbrains.kotlin.cli.common.ExitCode.OK) {
            return "{\"id\":" + r.id + ",\"ok\":false,\"error\":" + Json.str(log.toString(StandardCharsets.UTF_8).strip()) + ",\"compile_ms\":" + compileMs + "}";
        }
        // a fresh loader per request: the edited classes replace the old ones; Compose and Paparazzi stay shared
        List<URL> urls = new ArrayList<>(List.of(out.toUri().toURL()));
        if (lib != null) urls.add(lib.toUri().toURL());
        Path hostDir = host(work, cp);
        if (hostDir != null) urls.add(hostDir.toUri().toURL());
        // the libraries are on the worker's own classpath already (parent first): only the project's
        // code and the open file live in this loader
        try (URLClassLoader loader = new URLClassLoader(urls.toArray(URL[]::new), PreviewWorker.class.getClassLoader())) {
            Class<?> c = loader.loadClass(r.cls);
            StringBuilder images = new StringBuilder();
            int drawn = 0;
            for (String p : r.previews) {
                status(r, "Drawing " + (drawn + 1) + " of " + r.previews.size() + " previews", drawn, r.previews.size());
                drawn++;
                long t = System.nanoTime();
                ComposableMethod m = ComposableMethodKt.getDeclaredComposableMethod(c, p);
                m.asMethod().setAccessible(true);
                ComposableMethod hostMethod = hostDir == null ? null
                    : ComposableMethodKt.getDeclaredComposableMethod(loader.loadClass("droidlite.host.PreviewHostKt"), "PreviewHost", kotlin.jvm.functions.Function2.class);
                Thread.currentThread().setContextClassLoader(loader);
                sdk.prepare();
                try {
                    kotlin.jvm.functions.Function2<androidx.compose.runtime.Composer, Integer, kotlin.Unit> preview =
                        (composer, changed) -> { m.invoke(composer, null); return kotlin.Unit.INSTANCE; };
                    sdk.snapshot(hostMethod == null ? preview
                        : (composer, changed) -> { hostMethod.invoke(composer, null, preview); return kotlin.Unit.INSTANCE; });
                } finally {
                    sdk.teardown();
                }
                Path png = work.resolve(r.id + "-" + p + ".png");
                ImageIO.write(frame, "png", png.toFile());
                if (images.length() > 0) images.append(',');
                images.append("{\"name\":").append(Json.str(p)).append(",\"png\":").append(Json.str(png.toString()))
                    .append(",\"ms\":").append((System.nanoTime() - t) / 1_000_000).append('}');
            }
            return "{\"id\":" + r.id + ",\"ok\":true,\"images\":[" + images + "],\"compile_ms\":" + compileMs + "}";
        } finally {
            deleteTree(out); // the classes of one request are not needed after it
        }
    }

    static void deleteTree(Path p) {
        try (var walk = Files.walk(p)) {
            walk.sorted(Comparator.reverseOrder()).forEach(f -> f.toFile().delete());
        } catch (IOException ignored) {}
    }

    /** The SDK's android.jar stub leaves Build.FINGERPRINT null; Compose reads it ("robolectric" = a host JVM). */
    static void hostBuildFingerprint() throws Exception {
        java.lang.reflect.Field u = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        u.setAccessible(true);
        sun.misc.Unsafe unsafe = (sun.misc.Unsafe) u.get(null);
        java.lang.reflect.Field f = Class.forName("android.os.Build").getField("FINGERPRINT");
        unsafe.putObject(unsafe.staticFieldBase(f), unsafe.staticFieldOffset(f), "robolectric");
    }

    static String trace(Throwable e) {
        Throwable root = e;
        while (root.getCause() != null) root = root.getCause();
        StringBuilder s = new StringBuilder(root.toString());
        StackTraceElement[] st = root.getStackTrace();
        for (int i = 0; i < Math.min(6, st.length); i++) s.append("\n  at ").append(st[i]);
        return s.toString();
    }

    static synchronized void reply(SocketChannel out, String json) {
        try {
            java.nio.ByteBuffer b = java.nio.ByteBuffer.wrap((json + "\n").getBytes(StandardCharsets.UTF_8));
            while (b.hasRemaining()) out.write(b);
        } catch (IOException e) {
            System.err.println("reply: " + e); // Droid Lite went away: nothing to answer
        }
    }

    /** The real step, sent while a request runs: Droid Lite shows it under the loading card. */
    static void status(Request r, String text, int done, int total) {
        reply(r.out, "{\"id\":" + r.id + ",\"status\":" + Json.str(text) + ",\"done\":" + done + ",\"total\":" + total + "}");
    }

    static List<String> list(String property) {
        return Arrays.stream(System.getProperty(property, "").split(":")).filter(s -> !s.isEmpty()).toList();
    }

    static void sleep(long ms) { try { Thread.sleep(ms); } catch (InterruptedException ignored) {} }

    @SuppressWarnings("unchecked")
    static List<String> strings(Object o) { return o == null ? List.of() : (List<String>) (List<?>) o; }

    /** Just enough JSON for the requests: objects, arrays, strings, numbers, booleans, null. */
    static final class Json {
        final String s; int i;
        Json(String s) { this.s = s; }
        static Map<String, Object> parse(String s) { @SuppressWarnings("unchecked") var m = (Map<String, Object>) new Json(s).value(); return m; }
        Object value() {
            ws();
            char c = s.charAt(i);
            if (c == '{') { i++; Map<String, Object> m = new LinkedHashMap<>(); ws(); if (s.charAt(i) == '}') { i++; return m; }
                while (true) { ws(); String k = (String) value(); ws(); i++; m.put(k, value()); ws(); if (s.charAt(i++) == '}') return m; } }
            if (c == '[') { i++; List<Object> l = new ArrayList<>(); ws(); if (s.charAt(i) == ']') { i++; return l; }
                while (true) { l.add(value()); ws(); if (s.charAt(i++) == ']') return l; } }
            if (c == '"') { StringBuilder b = new StringBuilder(); i++;
                while (s.charAt(i) != '"') { char d = s.charAt(i++); if (d == '\\') { char e = s.charAt(i++);
                    switch (e) { case 'n' -> b.append('\n'); case 't' -> b.append('\t'); case 'u' -> { b.append((char) Integer.parseInt(s.substring(i, i + 4), 16)); i += 4; } default -> b.append(e); } }
                    else b.append(d); }
                i++; return b.toString(); }
            int st = i; while (i < s.length() && ",}] ".indexOf(s.charAt(i)) < 0) i++;
            String t = s.substring(st, i);
            return switch (t) { case "true" -> true; case "false" -> false; case "null" -> null; default -> Double.parseDouble(t); };
        }
        void ws() { while (i < s.length() && Character.isWhitespace(s.charAt(i))) i++; }
        static String str(String v) {
            StringBuilder b = new StringBuilder("\"");
            for (char c : v.toCharArray()) {
                switch (c) { case '"' -> b.append("\\\""); case '\\' -> b.append("\\\\"); case '\n' -> b.append("\\n"); case '\t' -> b.append("\\t");
                    default -> { if (c < 0x20) b.append(String.format("\\u%04x", (int) c)); else b.append(c); } }
            }
            return b.append('"').toString();
        }
    }
}
