// The nav in one place.
import { House, Settings } from "lucide"; // outer:nav-icons
import type { IconNode } from "lucide";

export interface NavItem {
  href: string;
  label: string;
  icon: IconNode;
}

export const NAV: readonly NavItem[] = [
  { href: "/", label: "Home", icon: House },
  // outer:nav-items — add_example inserts pages above this line
  { href: "/settings", label: "Settings", icon: Settings },
];
