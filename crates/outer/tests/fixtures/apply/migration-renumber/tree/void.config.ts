import { defineConfig } from "void/config";

export default defineConfig({
  cloudflare: {
    name: "fixture-app",
    d1_databases: [{ binding: "DB", database_name: "app-db", migrations_dir: "db/migrations" }],
    // outer:bindings — add_example inserts bindings above this line
  },
});
