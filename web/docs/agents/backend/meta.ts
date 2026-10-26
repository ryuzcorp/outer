import { defineMeta } from "blume";

export default defineMeta({
  title: "Backend",
  icon: "database",
  order: 2,
  pages: [
    "database",
    "files",
    "kv",
    "auth",
    "cron",
    "queues",
    "websockets",
    "sse",
    "email",
    "backups",
    "sandboxes",
  ],
});
