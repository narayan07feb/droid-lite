// Droid Lite's Compose preview worker: one warm JVM that compiles the open Kotlin file and renders its
// @Preview functions with layoutlib (Paparazzi's SDK). No Gradle. Droid Lite starts it detached on the
// first preview and talks to it over a Unix socket.
//
// Run: java -cp <base jars> PreviewWorker.java <socket path> <work dir>
//   system properties: paparazzi.layoutlib.runtime.root, paparazzi.layoutlib.resources.root,
//   droidlite.plugins (the Kotlin compiler plugin jars: Compose, parcelize; ":"-separated),
//   droidlite.compile.cp (the classpath the open file compiles against), droidlite.idle.seconds (600),
//   droidlite.write.timeout.ms (10000: a client that takes no reply for that long is dropped).
//
// Protocol, one JSON object per line each way (a tiny reader below, no JSON library):
//   -> {"id":7,"file":"/abs/Card.kt","sources":["/abs/Other.kt"],"class":"demo.CardKt","previews":["CardPreview"]}
//   <- {"id":7,"ok":true,"images":[{"name":"CardPreview","png":"/work/7-CardPreview.png","ms":48}],"compile_ms":210}
//   <- {"id":7,"ok":false,"error":"e: Card.kt:3:5 Unresolved reference 'Foo'."}
// One render at a time; the newest request wins (an older one waiting is dropped and answered "skipped").
// A request waiting for the renderer, and the one being drawn (a long compile says nothing for minutes),
// gets a status line every 10 s, so its client never takes this worker for hung.
// The socket: a worker that finds another answering on its path ends; it removes only the socket file
// it bound itself (a later worker may have bound a new one on the same path).
// <socket>.clients holds the number of connected clients (Droid Lite windows) and this worker's pid,
// so an engine that starts another worker never stops this one while a window uses it. The worker
// deletes it (when still its own) and its temp file as it ends; a start deletes dead workers' temp files.
// The work dir is shared by every worker of the preview folder: each worker's files carry its pid and
// a random token, and pictures older than an hour are deleted when a worker starts.
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
import java.nio.file.attribute.BasicFileAttributes;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import javax.imageio.ImageIO;

public class PreviewWorker {
    record Request(long id, String file, List<String> sources, String cls, List<Map<String, Object>> previews, List<String> cp,
        List<String> rjava, String key, Client out) {}

    static final AtomicReference<Request> next = new AtomicReference<>();
    static final Object wake = new Object();
    static volatile long lastWork = System.currentTimeMillis();
    static BufferedImage frame;
    /** The request being drawn, since when, and its last real step (the heartbeat repeats it). */
    static volatile Request current;
    static volatile long currentSince;
    static volatile String step = "Working";
    static volatile int stepDone, stepTotal;

    /** This worker's prefix for its files in the shared work dir: pid and a random token. */
    static final String TAG = ProcessHandle.current().pid() + "-" + Long.toHexString(new java.security.SecureRandom().nextLong() >>> 1);
    /** Requests this worker handled, for unique file names. */
    static final java.util.concurrent.atomic.AtomicLong handled = new java.util.concurrent.atomic.AtomicLong();
    static Path clientsFile;
    /** The project code dirs this worker compiled. */
    static final Set<Path> ownLibs = ConcurrentHashMap.newKeySet();

    public static void main(String[] a) throws Exception {
        Path sock = Path.of(a[0]);
        Path work = Path.of(a[1]);
        Files.createDirectories(work);
        sweepPictures(work, 3_600_000L);
        // two engines that each started a worker for one socket: the later one leaves the socket alone
        if (answers(sock)) {
            System.err.println("another preview worker answers on " + sock + "; this one ends");
            System.exit(0);
        }
        Files.deleteIfExists(sock); // a stopped worker's: nobody answers there
        hostBuildFingerprint();
        long idle = Long.getLong("droidlite.idle.seconds", 600) * 1000;
        Thread renderer = new Thread(() -> renderLoop(work), "render");
        renderer.setDaemon(true);
        renderer.start();
        Thread watchdog = new Thread(() -> {
            while (true) {
                sleep(10_000);
                // a request waiting behind another client's long draw, and the one being drawn, hear
                // from us every 10 s, so their clients do not take this worker for hung (they give up
                // after 120 s of silence)
                synchronized (wake) {
                    Request waiting = next.get();
                    if (waiting != null) send(waiting, "Waiting for another preview to finish", 0, 0);
                }
                Request drawing = current;
                if (drawing != null) {
                    long secs = (System.currentTimeMillis() - currentSince) / 1000;
                    send(drawing, step + " (still working, " + secs + " s)", stepDone, stepTotal);
                }
                if (next.get() == null && current == null && System.currentTimeMillis() - lastWork > idle) {
                    System.exit(0); // idle: give the memory back (the shutdown hook removes our socket)
                }
            }
        }, "idle");
        watchdog.setDaemon(true);
        Thread writes = new Thread(() -> {
            while (true) {
                sleep(1_000);
                long now = System.currentTimeMillis();
                for (Client c : clients) {
                    long since = c.writingSince;
                    if (since != 0 && now - since > WRITE_TIMEOUT) {
                        System.err.println("a client took no reply for " + WRITE_TIMEOUT + " ms: dropped");
                        c.close();
                    }
                }
            }
        }, "write-timeout");
        writes.setDaemon(true);
        writes.start();
        try (ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX)) {
            server.bind(UnixDomainSocketAddress.of(sock));
            Object bound = fileKey(sock);
            clientsFile = Path.of(sock + ".clients");
            sweepClientTemps(clientsFile);
            writeClients();
            // on idle exit and on SIGTERM: only the socket and clients files of this worker, never
            // a newer worker's on the same path
            Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                removeIfOwn(sock, bound);
                dropClientsFile();
            }));
            watchdog.start();
            System.err.println("preview worker ready on " + sock);
            while (true) {
                SocketChannel c = server.accept();
                Thread t = new Thread(() -> serve(c), "conn");
                t.setDaemon(true);
                t.start();
            }
        }
    }

    /** Deletes the PNGs in `work` older than `maxAgeMs`: a client copies its pictures as soon as they
     *  come, and one that went away never will. */
    static void sweepPictures(Path work, long maxAgeMs) {
        long cut = System.currentTimeMillis() - maxAgeMs;
        try (var files = Files.list(work)) {
            files.filter(f -> f.getFileName().toString().endsWith(".png")).forEach(f -> {
                try {
                    if (Files.getLastModifiedTime(f).toMillis() < cut) Files.deleteIfExists(f);
                } catch (IOException ignored) {}
            });
        } catch (IOException ignored) {}
    }

    /** "<clients> <pid>": the number of connected clients and whose count it is. */
    static String clientsText() {
        return clients.size() + " " + ProcessHandle.current().pid();
    }

    /** The number of connected clients into <socket>.clients, through a temp file and a rename. */
    static synchronized void writeClients() {
        if (clientsFile == null) return;
        Path tmp = Path.of(clientsFile + "." + TAG + ".tmp");
        try {
            Files.writeString(tmp, clientsText());
            Files.move(tmp, clientsFile, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
        } catch (IOException e) {
            try { Files.deleteIfExists(tmp); } catch (IOException ignored) {}
        }
    }

    /** As the worker ends: its temp file, and <socket>.clients when it is still this worker's (a
     *  newer worker on the same socket keeps its own). No write after this. */
    static synchronized void dropClientsFile() {
        Path f = clientsFile;
        if (f == null) return;
        clientsFile = null;
        try { Files.deleteIfExists(Path.of(f + "." + TAG + ".tmp")); } catch (IOException ignored) {}
        try {
            String[] t = Files.readString(f).trim().split(" ");
            if (t.length == 2 && t[1].equals(Long.toString(ProcessHandle.current().pid()))) Files.deleteIfExists(f);
        } catch (IOException ignored) {}
    }

    /** Deletes the `<socket>.clients.<pid>-<token>.tmp` files of workers that are gone (killed
     *  between the write and the rename). */
    static void sweepClientTemps(Path clientsFile) {
        String prefix = clientsFile.getFileName() + ".";
        try (var files = Files.list(clientsFile.toAbsolutePath().getParent())) {
            files.forEach(f -> {
                String n = f.getFileName().toString();
                if (!n.startsWith(prefix) || !n.endsWith(".tmp")) return;
                String tag = n.substring(prefix.length(), n.length() - 4);
                try {
                    long pid = Long.parseLong(tag.substring(0, tag.indexOf('-')));
                    if (ProcessHandle.of(pid).isEmpty()) Files.deleteIfExists(f);
                } catch (RuntimeException | IOException ignored) {}
            });
        } catch (IOException ignored) {}
    }

    /** Whether a worker answers on `sock`. */
    static boolean answers(Path sock) {
        try (SocketChannel c = SocketChannel.open(UnixDomainSocketAddress.of(sock))) {
            return true;
        } catch (IOException e) {
            return false;
        }
    }

    /** The file's identity (device and inode), or null. */
    static Object fileKey(Path p) {
        try {
            return Files.readAttributes(p, BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS).fileKey();
        } catch (IOException e) {
            return null;
        }
    }

    /** Removes `sock` only when it is still the file `bound` names (this worker's). */
    static boolean removeIfOwn(Path sock, Object bound) {
        Object now = fileKey(sock);
        if (bound == null || !bound.equals(now)) return false;
        try { return Files.deleteIfExists(sock); } catch (IOException e) { return false; }
    }

    /** How long a write to one client may block before that client is dropped. */
    static final long WRITE_TIMEOUT = Long.getLong("droidlite.write.timeout.ms", 10_000);
    static final Set<Client> clients = ConcurrentHashMap.newKeySet();

    /** One connection. Replies queue up and the client's own thread writes them, so a client that
     *  stops reading holds up nobody: not the renderer, not other clients. One whose write is stuck
     *  for WRITE_TIMEOUT is dropped (its channel closed). */
    static final class Client {
        final SocketChannel ch;
        final LinkedBlockingQueue<String> queue = new LinkedBlockingQueue<>();
        volatile long writingSince; // 0: not writing
        volatile boolean closed;

        Client(SocketChannel ch) {
            this.ch = ch;
            clients.add(this);
            writeClients();
            Thread w = new Thread(this::writeLoop, "write");
            w.setDaemon(true);
            w.start();
        }

        void send(String line) {
            if (!closed) queue.add(line);
        }

        void writeLoop() {
            try {
                while (!closed) {
                    String line = queue.poll(1, TimeUnit.SECONDS);
                    if (line == null) continue;
                    java.nio.ByteBuffer b = java.nio.ByteBuffer.wrap((line + "\n").getBytes(StandardCharsets.UTF_8));
                    writingSince = System.currentTimeMillis();
                    while (b.hasRemaining()) ch.write(b);
                    writingSince = 0;
                }
            } catch (IOException | InterruptedException e) {
                if (!closed) System.err.println("reply: " + e); // Droid Lite went away: nothing to answer
            }
            close();
        }

        void close() {
            closed = true;
            if (clients.remove(this)) writeClients();
            queue.clear();
            try { ch.close(); } catch (IOException ignored) {}
        }
    }

    static void serve(SocketChannel c) {
        // replies go through the client's own writer thread: Channels' stream wrappers share one lock,
        // so a reply would wait for the blocked read of the next request
        Client out = new Client(c);
        try (c; BufferedReader in = new BufferedReader(new InputStreamReader(Channels.newInputStream(c), StandardCharsets.UTF_8))) {
            String line;
            while ((line = in.readLine()) != null) {
                Map<String, Object> m = Json.parse(line);
                Request r = new Request(((Number) m.get("id")).longValue(), (String) m.get("file"),
                    strings(m.get("sources")), (String) m.get("class"), specs(m.get("previews")), strings(m.get("cp")),
                    strings(m.get("rjava")), m.get("key") == null ? "" : (String) m.get("key"), out);
                Request old = next.getAndSet(r);
                if (old != null) reply(old.out, "{\"id\":" + old.id + ",\"ok\":false,\"skipped\":true}");
                synchronized (wake) { wake.notifyAll(); }
            }
        } catch (Exception e) {
            if (!out.closed) System.err.println("connection: " + e);
        } finally {
            out.close();
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
            step = "Working";
            stepDone = stepTotal = 0;
            currentSince = System.currentTimeMillis();
            current = r;
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
            } finally {
                current = null;
            }
            lastWork = System.currentTimeMillis();
        }
    }

    /** The project's own code a preview needs (its R classes, then its source files), compiled once per
     *  key into work/lib-KEY and reused until a file changes. This worker's older lib dirs are deleted, and
     *  any other unused for an hour (the work dir is shared: another worker may be using its own). Null when none. */
    static Path ownCode(Request r, Path work, String cp) throws Exception {
        if (r.key.isEmpty() || (r.sources.isEmpty() && r.rjava.isEmpty())) return null;
        Path lib = work.resolve("lib-" + r.key);
        if (Files.exists(lib.resolve(".ok"))) {
            try { Files.setLastModifiedTime(lib.resolve(".ok"), java.nio.file.attribute.FileTime.fromMillis(System.currentTimeMillis())); } catch (IOException ignored) {}
            return lib;
        }
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
        ownLibs.add(lib);
        long cut = System.currentTimeMillis() - 3_600_000L;
        try (var dirs = Files.list(work)) {
            dirs.filter(d -> d.getFileName().toString().startsWith("lib-") && !d.equals(lib)).forEach(d -> {
                try {
                    if (ownLibs.remove(d) || Files.getLastModifiedTime(d.resolve(".ok")).toMillis() < cut) deleteTree(d);
                } catch (IOException ignored) {} // no .ok: another worker is compiling it
            });
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
        import androidx.compose.foundation.background
        import androidx.compose.foundation.layout.Box
        import androidx.compose.foundation.layout.height
        import androidx.compose.foundation.layout.width
        import androidx.compose.ui.Modifier
        import androidx.compose.ui.graphics.Color
        import androidx.compose.ui.unit.dp
        import androidx.compose.ui.platform.LocalInspectionMode
        import androidx.compose.ui.platform.LocalLifecycleOwner
        import androidx.core.app.ActivityOptionsCompat

        /** widthDp / heightDp and showBackground / backgroundColor of @Preview, like Android Studio. */
        @Composable
        fun PreviewFrame(widthDp: Int, heightDp: Int, background: Long, content: @Composable () -> Unit) {
            var m: Modifier = Modifier
            if (widthDp > 0) m = m.width(widthDp.dp)
            if (heightDp > 0) m = m.height(heightDp.dp)
            if (background != 0L) m = m.background(Color(background.toInt()))
            Box(m) { content() }
        }

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
                VM_OWNER
            ) {
                CMP_CONTEXT
                content()
            }
        }
        """;
    /** Compose Multiplatform resources read images and strings through an Android context that only an
     *  app start sets; previews set it with this effect (as Android Studio's preview does). */
    static final String CMP_CONTEXT = "@OptIn(org.jetbrains.compose.resources.ExperimentalResourceApi::class) org.jetbrains.compose.resources.PreviewContextConfigurationEffect()";
    /** A ViewModel store, as Android Studio's preview gives one: composables that take a viewModel()
     *  draw (one that needs Hilt's still cannot, as in Android Studio). */
    static final String VM_OWNER = "androidx.lifecycle.viewmodel.compose.LocalViewModelStoreOwner provides object : androidx.lifecycle.ViewModelStoreOwner { override val viewModelStore = androidx.lifecycle.ViewModelStore() },";
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
            // the richest host the project's libraries allow: Compose Multiplatform context and a
            // ViewModel store, then without one of them, then plain
            List<String> variants = new ArrayList<>();
            for (String cmp : List.of(CMP_CONTEXT, ""))
                for (String vm : List.of(VM_OWNER, "")) variants.add(HOST.replace("CMP_CONTEXT", cmp).replace("VM_OWNER", vm));
            for (String hostSource : variants) {
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
        // the request's id is its client's number: unique in the shared work dir only with this
        // worker's tag and its own count
        String name = TAG + "-" + handled.incrementAndGet();
        Path out = work.resolve("classes-" + name);
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
            for (Map<String, Object> spec : r.previews) {
                status(r, "Drawing " + (drawn + 1) + " of " + r.previews.size() + " previews", drawn, r.previews.size());
                drawn++;
                String fn = (String) spec.get("function");
                String title = spec.get("title") instanceof String s ? s : fn;
                long t = System.nanoTime();
                if (images.length() > 0) images.append(',');
                try {
                    ComposableMethod m = ComposableMethodKt.getDeclaredComposableMethod(c, fn);
                    m.asMethod().setAccessible(true);
                    Class<?> hostClass = hostDir == null ? null : loader.loadClass("droidlite.host.PreviewHostKt");
                    ComposableMethod hostMethod = hostClass == null ? null
                        : ComposableMethodKt.getDeclaredComposableMethod(hostClass, "PreviewHost", kotlin.jvm.functions.Function2.class);
                    ComposableMethod frameMethod = hostClass == null ? null
                        : ComposableMethodKt.getDeclaredComposableMethod(hostClass, "PreviewFrame", int.class, int.class, long.class, kotlin.jvm.functions.Function2.class);
                    int w = intOf(spec.get("widthDp")), h = intOf(spec.get("heightDp"));
                    long bg = spec.get("background") instanceof Number b ? b.longValue()
                        : Boolean.TRUE.equals(spec.get("showBackground")) ? (Boolean.TRUE.equals(spec.get("night")) ? 0xFF000000L : 0xFFFFFFFFL) : 0L;
                    Thread.currentThread().setContextClassLoader(loader);
                    sdk.prepare();
                    try {
                        // this picture's device (night mode, font scale, locale, screen size): it needs the
                        // session prepare() just opened
                        sdk.unsafeUpdateConfig(deviceFor(spec), "android:Theme.Material.NoActionBar.Fullscreen",
                            com.android.ide.common.rendering.api.SessionParams.RenderingMode.SHRINK);
                        kotlin.jvm.functions.Function2<androidx.compose.runtime.Composer, Integer, kotlin.Unit> preview =
                            (composer, changed) -> { m.invoke(composer, null); return kotlin.Unit.INSTANCE; };
                        kotlin.jvm.functions.Function2<androidx.compose.runtime.Composer, Integer, kotlin.Unit> framed = frameMethod == null ? preview
                            : (composer, changed) -> { frameMethod.invoke(composer, null, w, h, bg, preview); return kotlin.Unit.INSTANCE; };
                        sdk.snapshot(hostMethod == null ? framed
                            : (composer, changed) -> { hostMethod.invoke(composer, null, framed); return kotlin.Unit.INSTANCE; });
                    } finally {
                        sdk.teardown();
                    }
                    Path png = work.resolve(name + "-" + drawn + ".png");
                    ImageIO.write(frame, "png", png.toFile());
                    images.append("{\"name\":").append(Json.str(title)).append(",\"png\":").append(Json.str(png.toString()))
                        .append(",\"ms\":").append((System.nanoTime() - t) / 1_000_000).append('}');
                } catch (Throwable e) {
                    // one picture failing (e.g. a font scale the screen does not take) leaves the others
                    images.append("{\"name\":").append(Json.str(title)).append(",\"error\":").append(Json.str(trace(e))).append('}');
                }
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

    /** Never blocks: the client's own thread writes it. */
    static void reply(Client out, String json) {
        out.send(json);
    }

    /** The real step, sent while a request runs: Droid Lite shows it under the loading card. */
    static void status(Request r, String text, int done, int total) {
        if (r == current) {
            step = text;
            stepDone = done;
            stepTotal = total;
        }
        send(r, text, done, total);
    }

    static void send(Request r, String text, int done, int total) {
        reply(r.out, "{\"id\":" + r.id + ",\"status\":" + Json.str(text) + ",\"done\":" + done + ",\"total\":" + total + "}");
    }

    static List<String> list(String property) {
        return Arrays.stream(System.getProperty(property, "").split(":")).filter(s -> !s.isEmpty()).toList();
    }

    static void sleep(long ms) { try { Thread.sleep(ms); } catch (InterruptedException ignored) {} }

    @SuppressWarnings("unchecked")
    static List<Map<String, Object>> specs(Object o) {
        List<Map<String, Object>> out = new ArrayList<>();
        for (Object x : o == null ? List.of() : (List<Object>) o) {
            if (x instanceof Map) out.add((Map<String, Object>) x);
            else out.add(Map.of("function", x, "title", x)); // an older client: names only
        }
        return out;
    }

    /** The device for one picture: Paparazzi's PIXEL_5 with this preview's night mode, font scale,
     *  locale and screen size (`device = "spec:width=…dp,height=…dp"`), through the data class's copy
     *  with named parameters (kotlin-reflect). */
    static DeviceConfig deviceFor(Map<String, Object> spec) {
        DeviceConfig base = DeviceConfig.PIXEL_5;
        var copy = kotlin.reflect.full.KClasses.getMemberFunctions(kotlin.jvm.JvmClassMappingKt.getKotlinClass(DeviceConfig.class))
            .stream().filter(f -> f.getName().equals("copy")).findFirst().orElseThrow();
        Map<kotlin.reflect.KParameter, Object> args = new HashMap<>();
        double dpi = base.getDensity().getDpiValue();
        String device = spec.get("device") instanceof String d ? d : "";
        Integer wDp = specDp(device, "width"), hDp = specDp(device, "height");
        for (var p : copy.getParameters()) {
            if (p.getKind() == kotlin.reflect.KParameter.Kind.INSTANCE) { args.put(p, base); continue; }
            String n = p.getName();
            if ("nightMode".equals(n) && Boolean.TRUE.equals(spec.get("night"))) args.put(p, com.android.resources.NightMode.NIGHT);
            if ("fontScale".equals(n) && spec.get("fontScale") instanceof Number f) args.put(p, f.floatValue());
            if ("locale".equals(n) && spec.get("locale") instanceof String l) args.put(p, l);
            if ("screenWidth".equals(n) && wDp != null) args.put(p, (int) Math.round(wDp * dpi / 160));
            if ("screenHeight".equals(n) && hDp != null) args.put(p, (int) Math.round(hDp * dpi / 160));
        }
        return (DeviceConfig) copy.callBy(args);
    }

    /** `spec:width=411dp,height=891dp` -> 411 for "width". */
    static Integer specDp(String device, String key) {
        for (String part : device.replace("spec:", "").split(",")) {
            String[] kv = part.trim().split("=");
            if (kv.length == 2 && kv[0].trim().equals(key)) {
                try { return (int) Double.parseDouble(kv[1].trim().replace("dp", "")); } catch (NumberFormatException e) { return null; }
            }
        }
        return null;
    }

    static int intOf(Object o) { return o instanceof Number n ? n.intValue() : 0; }

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
