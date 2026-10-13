import {
  readFile,
  mkdir,
  copyFile,
  writeFile,
  realpath,
  stat,
} from "node:fs/promises";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { load } from "cheerio";
import { root, publicFiles, revision, siteData, renderStats } from "./data.mjs";

export function quillContent() {
  let config;
  let building;
  const output = () => path.resolve(root, config.build.outDir);
  // Share a coherent snapshot across the four pages in a production build.
  const data = () => building || siteData();
  function middleware(server, preview = false) {
    return async (req, res, next) => {
      try {
        const url = new URL(req.url, "http://localhost");
        const pathname = decodeURIComponent(url.pathname);
        const send = (body, type) => {
          res.setHeader("Cache-Control", "no-store");
          res.setHeader("Content-Type", type);
          res.end(req.method === "HEAD" ? undefined : body);
        };
        if (pathname === "/project-status.json") {
          const value = preview
            ? await readFile(path.join(output(), "project-status.json"))
            : JSON.stringify({ revision: revision() });
          return send(value, "application/json");
        }
        if (pathname === "/site-data.json") {
          const value = preview
            ? await readFile(path.join(output(), "site-data.json"))
            : JSON.stringify(await data());
          return send(value, "application/json");
        }
        if (!/\.(md|json)$/i.test(pathname)) return next();
        const base = preview ? output() : root;
        const name = pathname.slice(1);
        if (!preview && !(await publicFiles()).includes(name)) {
          res.statusCode = 404;
          return send("Document not found", "text/plain; charset=utf-8");
        }
        const file = await realpath(path.resolve(base, name));
        if (!file.startsWith(base + path.sep) || !(await stat(file)).isFile()) {
          res.statusCode = 404;
          return send("Document not found", "text/plain; charset=utf-8");
        }
        if (url.searchParams.get("raw") === "1")
          return send(await readFile(file), "text/plain; charset=utf-8");
        const template = await readFile(
          path.join(base, "document.html"),
          "utf8",
        );
        return send(
          preview
            ? template
            : await server.transformIndexHtml("/document.html", template),
          "text/html; charset=utf-8",
        );
      } catch (error) {
        res.statusCode = error.code === "ENOENT" ? 404 : 500;
        res.setHeader("Content-Type", "text/plain; charset=utf-8");
        res.end(
          res.statusCode === 404
            ? "Document not found"
            : `Website source error: ${error.message}`,
        );
        if (res.statusCode === 500) server.config.logger.error(error.stack);
      }
    };
  }
  return {
    name: "quill-repository-content",
    configResolved(value) {
      config = value;
    },
    buildStart() {
      if (config.command === "build") building = siteData();
    },
    transformIndexHtml: {
      order: "pre",
      async handler(html) {
        const $ = load(renderStats(html, await data()));
        // Vite owns script loading, bundling and reloads.
        $("script[src]").each((_, script) => {
          const el = $(script);
          const src = el.attr("src");
          if (!src.includes("/vendor/"))
            el.attr("type", "module").removeAttr("defer");
        });
        return $.html();
      },
    },
    configureServer(server) {
      server.middlewares.use(middleware(server));
      let timer;
      const reload = (file) => {
        if (
          !/\.(md|json|html|js|svg|png|jpe?g)$/i.test(file) &&
          !file.includes(".git/")
        )
          return;
        if (file.includes("/node_modules/") || file.includes("/target/"))
          return;
        clearTimeout(timer);
        timer = setTimeout(
          () => server.ws.send({ type: "full-reload", path: "*" }),
          150,
        );
      };
      server.watcher
        .on("add", reload)
        .on("change", reload)
        .on("unlink", reload);
      // HEAD and refs are otherwise excluded by Vite's repository watcher.
      const gitPaths = ["HEAD", "refs", "packed-refs"].map((name) =>
        execFileSync("git", ["rev-parse", "--git-path", name], {
          cwd: root,
          encoding: "utf8",
        }).trim(),
      );
      // fs.watch handles the small Git metadata surface without watching objects.
      let gitWatchers = [];
      import("node:fs").then(({ watch, existsSync }) => {
        gitWatchers = gitPaths
          .map((p) => path.resolve(root, p))
          .filter(existsSync)
          .map((p) => watch(p, { recursive: true }, () => reload(".git/HEAD")));
      });
      server.httpServer?.once("close", () => {
        clearTimeout(timer);
        gitWatchers.forEach((w) => w.close());
      });
    },
    configurePreviewServer(server) {
      server.middlewares.use(middleware(server, true));
    },
    async closeBundle() {
      if (config.command !== "build") return;
      const snapshot = await building;
      for (const name of await publicFiles()) {
        const destination = path.join(output(), name);
        await mkdir(path.dirname(destination), { recursive: true });
        await copyFile(path.join(root, name), destination);
      }
      await writeFile(
        path.join(output(), "site-data.json"),
        JSON.stringify(snapshot, null, 2) + "\n",
      );
      await writeFile(
        path.join(output(), "project-status.json"),
        JSON.stringify({ revision: snapshot.revision }) + "\n",
      );
      await copyFile(
        path.join(root, "website/_headers"),
        path.join(output(), "_headers"),
      );
    },
  };
}
