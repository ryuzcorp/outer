// Console icon set: Lucide glyphs rendered through a tiny Ilha component.
//
// The islands are Ilha JSX, not Astro components, so `@lucide/astro` does
// not fit; the `lucide` vanilla package's per-icon `IconNode` exports
// (`[tag, attrs][]` — the same shape `createElement` consumes, see
// lucide's ESM dist) render here with no DOM, no CDN and no
// `createIcons()` DOM scanning, so the strict console CSP stays intact.
// Every icon is a 24x24 stroke glyph at 1.5px (the reference weight);
// pass `size` for pixel height/width.
//
// `OuterMark` is our own brand mark (three dots), not a Lucide glyph.
import { h } from "ilha";
import {
  Activity,
  ArrowLeft,
  ArrowUpDown,
  BookOpen,
  ChartLine,
  Check,
  ChevronDown,
  CodeXml,
  ChevronLeft,
  ChevronRight,
  Copy,
  Database,
  Eye,
  EyeOff,
  Filter,
  KeyRound,
  LayoutGrid,
  Lock,
  LogOut,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  Plus,
  RefreshCw,
  Rocket,
  Search,
  Settings,
  ShieldCheck,
  Table,
  Terminal,
  Ticket,
  TriangleAlert,
  UserRound,
  Users,
  X,
  type IconNode,
} from "lucide";

function Lucide({
  node,
  size = 18,
  label,
}: {
  node: IconNode;
  size?: number;
  label?: string;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.5"
      stroke-linecap="round"
      stroke-linejoin="round"
      role={label ? "img" : "presentation"}
      aria-label={label}
      aria-hidden={label ? undefined : "true"}
    >
      {node.map(([tag, attrs], i) => h(tag, { ...attrs, key: i }))}
    </svg>
  );
}

export function OuterMark({ size = 22 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label="Outer home"
    >
      <circle cx="12" cy="5.5" r="2" />
      <circle cx="5.5" cy="17.5" r="2" />
      <circle cx="18.5" cy="17.5" r="2" />
    </svg>
  );
}

export function OverviewIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={LayoutGrid} size={size} />;
}

export function MetricsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ChartLine} size={size} />;
}

export function ActionsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Activity} size={size} />;
}

export function DeploysIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Rocket} size={size} />;
}
export function CodeIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={CodeXml} size={size} />;
}

export function LogsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Terminal} size={size} />;
}

export function ErrorsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={TriangleAlert} size={size} />;
}

export function SecretsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={KeyRound} size={size} />;
}

export function SettingsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Settings} size={size} />;
}
export function CollapseIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={PanelLeftClose} size={size} />;
}

export function ExpandIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={PanelLeftOpen} size={size} />;
}

export function MenuIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Menu} size={size} />;
}

export function DocsIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={BookOpen} size={size} />;
}

export function SignOutIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={LogOut} size={size} />;
}

export function CopyIcon({ size = 14 }: { size?: number }) {
  return <Lucide node={Copy} size={size} />;
}

export function CheckIcon({ size = 14 }: { size?: number }) {
  return <Lucide node={Check} size={size} />;
}

export function PlusIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Plus} size={size} />;
}

export function CloseIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={X} size={size} />;
}

export function ChevronDownIcon({ size = 14 }: { size?: number }) {
  return <Lucide node={ChevronDown} size={size} />;
}

export function DatabaseIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Database} size={size} />;
}

export function TicketIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Ticket} size={size} />;
}

export function ShieldIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ShieldCheck} size={size} />;
}

export function EyeIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Eye} size={size} label="Show" />;
}

export function EyeOffIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={EyeOff} size={size} label="Hide" />;
}

export function SearchIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Search} size={size} />;
}

export function FilterIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Filter} size={size} />;
}

export function SortIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ArrowUpDown} size={size} />;
}

export function RefreshIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={RefreshCw} size={size} />;
}

export function ChevronLeftIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ChevronLeft} size={size} />;
}

export function ChevronRightIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ChevronRight} size={size} />;
}

export function TableIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Table} size={size} />;
}

export function BackIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={ArrowLeft} size={size} />;
}

export function LockIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Lock} size={size} />;
}

export function KeyIcon({ size = 14 }: { size?: number }) {
  return <Lucide node={KeyRound} size={size} />;
}

export function AccountIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={UserRound} size={size} />;
}

export function UsersIcon({ size = 18 }: { size?: number }) {
  return <Lucide node={Users} size={size} />;
}
