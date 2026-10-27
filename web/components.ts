import { defineComponents } from "blume";
import Header from "./components/blume/Header.astro";
import NavTree from "./components/blume/NavTree.astro";
import SectionSidebar from "./components/SectionSidebar.astro";

// Docs chrome, owned via `blume add` so the copies render exactly like the
// built-ins until edited (they import the rest from `blume/*`):
// - Header: tabs out (they live in the sidebar now), Discord + GitHub in.
// - Sidebar: the Guide / For agents / Reference section switcher above the
//   tree. MobileNav stays the plain tree: the drawer's own tabs row above it
//   already switches sections on phones.
export default defineComponents({
  layout: {
    Header,
    MobileNav: NavTree,
    Sidebar: SectionSidebar,
  },
});
