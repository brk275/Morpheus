/*
Copyright 2021-2024 New Vector Ltd.

SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
Please see LICENSE in the repository root for full details.
*/

import {
  type PluginOption,
  type ConfigEnv,
  type UserConfig,
} from "vite";
import svgrPlugin from "vite-plugin-svgr";
import { createHash } from "node:crypto";

import { nodePolyfills } from "vite-plugin-node-polyfills";
import wasm from "vite-plugin-wasm";
import babel from "@rolldown/plugin-babel";
import react, { reactCompilerPreset } from "@vitejs/plugin-react";

// Deterministic equivalent of this pinned full-package template only.
// Avoid vite-plugin-html's upward .env/config search and EJS execution.
function closedHtml(): PluginOption {
  return {
    name: "morpheus-closed-index-html",
    transformIndexHtml: {
      order: "pre",
      handler(html) {
        if (createHash("sha256").update(html).digest("hex") !== "d8d05a9226b6e563d5c2ce19214cf1f26e1e9381ae6c3d85d1a5327a1aacfeb1") {
          throw new Error("Modified peer index template changed");
        }
        const opening = '<% if (packageType === "full") { %>';
        const closing = "<% } %>";
        const brand = "<%- brand %>";
        if (html.split(opening).length !== 3 || html.split(closing).length !== 3 || html.split(brand).length !== 6) {
          throw new Error("Modified peer index template is ambiguous");
        }
        const rendered = html.replaceAll(opening, "").replaceAll(closing, "").replaceAll(brand, "Element Call");
        if (rendered.includes("<%")) throw new Error("Modified peer index has unsupported template code");
        return { html: rendered, tags: [{ tag: "script", attrs: { type: "module", src: "src/main.tsx" }, injectTo: "body" }] };
      },
    },
  };
}

export const vitePluginsConfig = ({
  mode,
  html = true,
}: Pick<ConfigEnv, "mode"> & {
  /**
   * Whether to inject Element Call's entry point into the HTML page. Builds
   * that produce a library, or serve a page of their own, must not have this:
   * it would pull the standalone app in alongside whatever they are building.
   */
  html?: boolean;
}): UserConfig => {
  const plugins: PluginOption[] = [
    babel({
      presets: [reactCompilerPreset()],
    }),
    react(),
    wasm(),
    nodePolyfills({
      // Enables the 'events' module, which is required by the matrix-js-sdk
      include: ["events"],
    }),
    svgrPlugin({
      svgrOptions: {
        // This enables ref forwarding on SVGR components, which is needed, for
        // example, to make tooltips on icons work
        ref: true,
        runtimeConfig: false,
      },
    }),
  ];


  if (html && !process.env.STORYBOOK && !process.env.VITEST) {
    plugins.push(
      closedHtml(),
    );
  }

  return { plugins };
};
// https://vitejs.dev/config/
// Modified type helper from defineConfig to allow for packageType (see defineConfig from vite)
export default ({
  mode,
  packageType,
}: ConfigEnv & { packageType?: "full" | "embedded" }): UserConfig => {
  // Environment variables with the VITE_ prefix are accessible at runtime.
  // So, we set this to allow for build/package specific behavior.
  // In future we might be able to do what is needed via code splitting at
  // build time.
  process.env.VITE_PACKAGE = packageType ?? "full";

  return {
    ...vitePluginsConfig({ mode }),
    envDir: false,
    worker: {
      format: "es",
    },
    build: {
      minify: mode === "production" ? true : false,
      sourcemap: true,
      rollupOptions: {
        output: {
          assetFileNames: ({ originalFileNames }): string => {
            if (originalFileNames) {
              for (const name of originalFileNames) {
                // Custom asset name for locales to include the locale code in the filename
                const match = name.match(/locales\/([^/]+)\/(.+)\.json$/);
                if (match) {
                  const [, locale, filename] = match;
                  return `assets/${locale}-${filename}-[hash].json`;
                }
              }
            }

            // Default naming fallback
            return "assets/[name]-[hash][extname]";
          },
        },
      },
    },
    resolve: {
      alias: {
        // matrix-widget-api has its transpiled lib/index.js as its entry point,
        // which Vite for some reason refuses to work with, so we point it to
        // src/index.ts instead
        "matrix-widget-api": "matrix-widget-api/src/index.ts",
      },
      dedupe: [
        "react",
        "react-dom",
        "matrix-js-sdk",
        "react-use-measure",
        // These packages modify the document based on some module-level global
        // state, and don't play nicely with duplicate copies of themselves
        // https://github.com/radix-ui/primitives/issues/1241#issuecomment-1847837850
        "@radix-ui/react-focus-guards",
        "@radix-ui/react-dismissable-layer",
      ],
    },
  };
};
