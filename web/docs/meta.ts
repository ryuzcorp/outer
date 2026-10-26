import { defineMeta } from "blume";

// The Guide tab owns the root: Welcome first, then the groups below.
// Groups order themselves via their own meta.ts. The Agents and
// Reference tabs scope their own folders.
export default defineMeta({
  pages: ["index"],
});
