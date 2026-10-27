import { defineConfig } from "void/config";

export default defineConfig({
  cloudflare: {
    name: "fixture-site",
    assets: { not_found_handling: "404-page" },
    // outer:bindings — add_example inserts bindings above this line
  },
});
