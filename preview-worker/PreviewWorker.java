// Droid Lite's Compose preview worker: one warm JVM that compiles the open Kotlin file and renders its
// @Preview functions with layoutlib (Paparazzi's SDK). No Gradle. Droid Lite starts it detached on the
// first preview and talks to it over a Unix socket.
//
// Run: java -cp <base jars> PreviewWorker.java <socket path> <work dir>
//   system properties: paparazzi.layoutlib.runtime.root, paparazzi.layoutlib.resources.root,
//   droidlite.compose.plugin (the Kotlin Compose compiler plugin jar),
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
    record Request(long id, String file, List<String> sources, String cls, List<String> previews, SocketChannel out) {}

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
                    strings(m.get("sources")), (String) m.get("class"), strings(m.get("previews")), out);
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
                    Environment env = new Environment(work.toString(), "droidlite.preview", 35,
                        List.of(), List.of(), List.of(), List.of(), List.of(), List.of());
                    sdk = new PaparazziSdk(env, DeviceConfig.PIXEL_5, img -> { frame = img; return kotlin.Unit.INSTANCE; });
                    sdk.setup();
                }
                reply(r.out, handle(r, work, sdk));
            } catch (Throwable e) {
                reply(r.out, "{\"id\":" + r.id + ",\"ok\":false,\"error\":" + Json.str(trace(e)) + "}");
            }
            lastWork = System.currentTimeMillis();
        }
    }

    static String handle(Request r, Path work, PaparazziSdk sdk) throws Exception {
        Path out = work.resolve("classes-" + r.id);
        long t0 = System.nanoTime();
        List<String> args = new ArrayList<>();
        args.add(r.file);
        args.addAll(r.sources);
        args.addAll(List.of("-d", out.toString(), "-classpath", System.getProperty("droidlite.compile.cp"),
            "-no-stdlib", "-no-reflect", "-jvm-target", "17", "-nowarn", "-module-name", "preview",
            "-Xplugin=" + System.getProperty("droidlite.compose.plugin")));
        ByteArrayOutputStream log = new ByteArrayOutputStream();
        var code = new org.jetbrains.kotlin.cli.jvm.K2JVMCompiler().exec(new PrintStream(log, true, StandardCharsets.UTF_8), args.toArray(String[]::new));
        long compileMs = (System.nanoTime() - t0) / 1_000_000;
        if (code != org.jetbrains.kotlin.cli.common.ExitCode.OK) {
            return "{\"id\":" + r.id + ",\"ok\":false,\"error\":" + Json.str(log.toString(StandardCharsets.UTF_8).strip()) + ",\"compile_ms\":" + compileMs + "}";
        }
        // a fresh loader per request: the edited classes replace the old ones; Compose and Paparazzi stay shared
        try (URLClassLoader loader = new URLClassLoader(new URL[] {out.toUri().toURL()}, PreviewWorker.class.getClassLoader())) {
            Class<?> c = loader.loadClass(r.cls);
            StringBuilder images = new StringBuilder();
            for (String p : r.previews) {
                long t = System.nanoTime();
                ComposableMethod m = ComposableMethodKt.getDeclaredComposableMethod(c, p);
                m.asMethod().setAccessible(true);
                Thread.currentThread().setContextClassLoader(loader);
                sdk.prepare();
                try {
                    sdk.snapshot((composer, changed) -> { m.invoke(composer, null); return kotlin.Unit.INSTANCE; });
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
        }
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
