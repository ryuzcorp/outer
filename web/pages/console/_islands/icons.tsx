// Thin-stroke icon set for the console shell.
// Ported from Noite's $lib/ui/icons (1.5px outline SVGs, currentColor):
// narrowed to the console's set, no icon font, no CDN — CSP is self-only.
// Every icon is a 24×24 stroke glyph; pass `size` for pixel height/width.
export function Icon({
  size = 18,
  children,
  label,
}: {
  size?: number;
  children?: unknown;
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
      {children}
    </svg>
  );
}

export function OuterMark({ size = 22 }: { size?: number }) {
  return (
    <Icon size={size} label="Outer home">
      <circle cx="12" cy="5.5" r="2" fill="currentColor" stroke="none" />
      <circle cx="5.5" cy="17.5" r="2" fill="currentColor" stroke="none" />
      <circle cx="18.5" cy="17.5" r="2" fill="currentColor" stroke="none" />
    </Icon>
  );
}

export function OverviewIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <rect x="3" y="3" width="7.5" height="7.5" rx="1.5" />
      <rect x="13.5" y="3" width="7.5" height="7.5" rx="1.5" />
      <rect x="3" y="13.5" width="7.5" height="7.5" rx="1.5" />
      <rect x="13.5" y="13.5" width="7.5" height="7.5" rx="1.5" />
    </Icon>
  );
}

export function MetricsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M3 3v16a2 2 0 0 0 2 2h16" />
      <path d="m7 14 4-4 3 3 5-6" />
    </Icon>
  );
}

export function ActionsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M22 12h-4l-3 9L9 3l-3 9H2" />
    </Icon>
  );
}

export function DeploysIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M4 17V7a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v10" />
      <path d="M8 21h8" />
      <path d="M12 5v9" />
      <path d="m8.5 10.5 3.5 3.5 3.5-3.5" />
    </Icon>
  );
}

export function LogsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="m4 17 6-6-6-6" />
      <path d="M12 19h8" />
    </Icon>
  );
}

export function ErrorsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="m10.4 3.6 9.2 16a1 1 0 0 1-.9 1.4H5.3a1 1 0 0 1-.9-1.4l9.2-16a1 1 0 0 1 1.8 0Z" />
      <path d="M12 9v4" />
      <path d="M12 17h.01" />
    </Icon>
  );
}

export function SecretsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <circle cx="8" cy="15" r="4" />
      <path d="m10.9 12.1 8.6-8.6" />
      <path d="m18 5 2 2" />
      <path d="m15 8 2 2" />
    </Icon>
  );
}

export function SettingsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.9 2.9l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1a2 2 0 1 1-2.9-2.9l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.6-1.1 1.7 1.7 0 0 0-.3-1.9l-.1-.1a2 2 0 1 1 2.9-2.9l.1.1a1.7 1.7 0 0 0 1.9.3h0a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5h0a1.7 1.7 0 0 0 1.9-.3l.1-.1a2 2 0 1 1 2.9 2.9l-.1.1a1.7 1.7 0 0 0-.3 1.9v0a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z" />
    </Icon>
  );
}

export function CollapseIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <rect x="3" y="3" width="18" height="18" rx="2" />
      <path d="m14 9-3 3 3 3" />
      <path d="M9 5v14" />
    </Icon>
  );
}

export function MenuIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M4 6h16" />
      <path d="M4 12h16" />
      <path d="M4 18h16" />
    </Icon>
  );
}

export function DocsIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M2 4h6a4 4 0 0 1 4 4v12a3 3 0 0 0-3-3H2Z" />
      <path d="M22 4h-6a4 4 0 0 0-4 4v12a3 3 0 0 1 3-3h7Z" />
    </Icon>
  );
}

export function SignOutIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" />
      <path d="m16 17 5-5-5-5" />
      <path d="M21 12H9" />
    </Icon>
  );
}

export function CopyIcon({ size = 14 }: { size?: number }) {
  return (
    <Icon size={size}>
      <rect x="9" y="9" width="12" height="12" rx="2" />
      <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
    </Icon>
  );
}

export function CheckIcon({ size = 14 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M20 6 9 17l-5-5" />
    </Icon>
  );
}

export function PlusIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M12 5v14" />
      <path d="M5 12h14" />
    </Icon>
  );
}

export function CloseIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </Icon>
  );
}

export function ChevronDownIcon({ size = 14 }: { size?: number }) {
  return (
    <Icon size={size}>
      <path d="m6 9 6 6 6-6" />
    </Icon>
  );
}

export function DatabaseIcon({ size = 18 }: { size?: number }) {
  return (
    <Icon size={size}>
      <ellipse cx="12" cy="5" rx="8" ry="3" />
      <path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5" />
      <path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3" />
    </Icon>
  );
}
