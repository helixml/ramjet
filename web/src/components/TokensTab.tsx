import { useEffect, useMemo, useState } from "react"
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Search,
  Ban,
  Check,
  Copy,
  KeyRound,
  Pencil,
  Plus,
  ShieldOff,
  Trash2,
} from "lucide-react"
import { Modal, Segmented, inputClass } from "@/components/Controls"
import { detailHref } from "@/components/UsageParts"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePoll } from "@/hooks/usePoll"
import {
  UnauthorizedError,
  createToken,
  deleteToken,
  fetchUsageSeries,
  listTokens,
  localTzOffsetMin,
  revokeToken,
  tokenStatusOf,
  updateToken,
  SERVER_OTHER_KEY,
  type ApiToken,
  type CreatedToken,
  type SeriesResponse,
  type StatsTotal,
  type TokenStatus,
} from "@/lib/accounting"
import { fmtCount, fmtDateTime, fmtPct, fmtRelative } from "@/lib/format"

const DAY = 86_400_000

const STATUS_STYLE: Record<TokenStatus, { color: string; label: string }> = {
  active: { color: "var(--status-good)", label: "active" },
  expired: { color: "var(--status-warning)", label: "expired" },
  revoked: { color: "var(--status-critical)", label: "revoked" },
}

export function StatusBadge({ status }: { status: TokenStatus }) {
  const style = STATUS_STYLE[status]
  return (
    <Badge variant="outline" className="whitespace-nowrap">
      <span aria-hidden className="size-1.5 rounded-full" style={{ background: style.color }} />
      {style.label}
    </Badge>
  )
}

export function When({ t, now, empty = "—" }: { t: number | null; now: number; empty?: string }) {
  if (t == null) return <span className="text-faint-foreground">{empty}</span>
  return (
    <span title={fmtDateTime(t)} className="whitespace-nowrap">
      {fmtRelative(t, now)}
    </span>
  )
}

/** Token rows rendered per page; the list can hold ten thousand. */
const PAGE_SIZE = 50
/** One series request returns this many individual principals (backend max). */
const USAGE_LIMIT = 500

type StatusFilter = "all" | TokenStatus

export type TokenDialogState =
  | { kind: "create" }
  | { kind: "edit"; token: ApiToken }
  | { kind: "revoke"; token: ApiToken }
  | { kind: "delete"; token: ApiToken }
  | null

/**
 * 24h usage for one principal, or what is known when it is absent: if the
 * server folded groups into `__other`, an absent token may be in there
 * (unknown, "—"); otherwise every principal with traffic was returned, so
 * an absent one truly had none.
 */
export function usageLookup(response: SeriesResponse | null) {
  const byKey = new Map((response?.totals ?? []).map((total) => [total.key, total]))
  const folded = byKey.has(SERVER_OTHER_KEY)
  return {
    byKey,
    get: (key: string): StatsTotal | "none" | "unknown" =>
      byKey.get(key) ?? (response == null || folded ? "unknown" : "none"),
  }
}

export function TokensTab({ onUnauthorized }: { onUnauthorized: () => void }) {
  const list = usePoll(listTokens, "tokens", 30_000, onUnauthorized)
  const usage = usePoll(
    () => {
      const now = Date.now()
      return fetchUsageSeries({
        granularity: "hour",
        from_ms: now - DAY,
        to_ms: now,
        group_by: "token",
        tz_offset_min: localTzOffsetMin(),
        limit: USAGE_LIMIT,
      })
    },
    "usage24h",
    60_000,
    onUnauthorized,
  )
  const [dialog, setDialog] = useState<TokenDialogState>(null)
  const [created, setCreated] = useState<CreatedToken | null>(null)
  const [query, setQuery] = useState("")
  const [status, setStatus] = useState<StatusFilter>("all")
  const [page, setPage] = useState(0)

  const lookup = useMemo(() => usageLookup(usage.data), [usage.data])
  const now = Date.now()
  const tokens = useMemo(() => list.data?.tokens ?? [], [list.data])
  const counts = useMemo(() => {
    const at = Date.now()
    return tokens.reduce(
      (acc, token) => {
        acc[tokenStatusOf(token, at)] += 1
        return acc
      },
      { active: 0, expired: 0, revoked: 0 } as Record<TokenStatus, number>,
    )
  }, [tokens])
  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase()
    const at = Date.now()
    return tokens.filter(
      (token) =>
        (status === "all" || tokenStatusOf(token, at) === status) &&
        (!needle ||
          token.name.toLowerCase().includes(needle) ||
          token.description.toLowerCase().includes(needle) ||
          token.prefix.toLowerCase().includes(needle)),
    )
  }, [tokens, query, status])
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  // A narrower search can leave the current page past the end.
  useEffect(() => {
    if (page > pages - 1) setPage(pages - 1)
  }, [page, pages])
  const visible = filtered.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE)
  const disabled = list.missing || (list.data != null && !list.data.enabled)

  if (list.loading && list.data == null && !list.error) {
    return (
      <Card>
        <CardContent className="flex flex-col gap-2 pt-4">
          {Array.from({ length: 4 }, (_, index) => (
            <Skeleton key={index} className="h-9 w-full" />
          ))}
        </CardContent>
      </Card>
    )
  }

  return (
    <div className="flex flex-col gap-3">
      {disabled ? <DisabledState /> : null}
      {list.error && !list.missing ? (
        <div className="text-xs" style={{ color: "var(--status-critical)" }}>
          ⚠ {list.error}
        </div>
      ) : null}

      {created ? <SecretPanel created={created} onDismiss={() => setCreated(null)} /> : null}

      {list.missing ? null : (
        <Card>
          <CardHeader className="flex-row flex-wrap items-start justify-between gap-2">
            <div className="flex flex-col gap-1">
              <CardTitle>API tokens</CardTitle>
              <CardDescription>
                {tokens.length === 0
                  ? "no tokens minted yet"
                  : `${tokens.length.toLocaleString("en-US")} tokens · ${counts.active.toLocaleString("en-US")} active · ${counts.expired.toLocaleString("en-US")} expired · ${counts.revoked.toLocaleString("en-US")} revoked · usage columns cover the last 24 h · click a token for its detail`}
              </CardDescription>
            </div>
            {disabled ? null : (
              <Button onClick={() => setDialog({ kind: "create" })}>
                <Plus /> New token
              </Button>
            )}
          </CardHeader>
          <CardContent className="flex flex-col gap-2">
            {tokens.length === 0 ? (
              <div className="flex flex-col items-center gap-2 py-8 text-center">
                <KeyRound className="text-faint-foreground size-6" />
                <div className="text-muted-foreground text-xs">
                  Mint a token per client so its usage, cache hit ratio, and latency can be
                  told apart from everyone else's.
                </div>
              </div>
            ) : (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <label className="relative">
                    <Search className="text-faint-foreground pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2" />
                    <input
                      type="search"
                      aria-label="Search tokens"
                      placeholder="search name, description, rjk_ prefix"
                      value={query}
                      onChange={(event) => {
                        setQuery(event.target.value)
                        setPage(0)
                      }}
                      className={`${inputClass} h-7 w-[280px] py-0 pl-7`}
                    />
                  </label>
                  <Segmented
                    label="Status"
                    value={status}
                    onChange={(next) => {
                      setStatus(next)
                      setPage(0)
                    }}
                    options={[
                      { value: "all", label: `all ${tokens.length.toLocaleString("en-US")}` },
                      { value: "active", label: `active ${counts.active.toLocaleString("en-US")}` },
                      { value: "expired", label: `expired ${counts.expired.toLocaleString("en-US")}` },
                      { value: "revoked", label: `revoked ${counts.revoked.toLocaleString("en-US")}` },
                    ]}
                  />
                  <Pager page={page} pages={pages} total={filtered.length} onPage={setPage} />
                </div>
                {filtered.length === 0 ? (
                  <div className="text-faint-foreground py-6 text-center text-xs">
                    no token matches this search
                  </div>
                ) : (
                  <TokenTable
                    tokens={visible}
                    usage={lookup.get}
                    now={now}
                    readOnly={disabled}
                    onEdit={(token) => setDialog({ kind: "edit", token })}
                    onRevoke={(token) => setDialog({ kind: "revoke", token })}
                    onDelete={(token) => setDialog({ kind: "delete", token })}
                  />
                )}
                {pages > 1 ? (
                  <div className="flex justify-end">
                    <Pager page={page} pages={pages} total={filtered.length} onPage={setPage} />
                  </div>
                ) : null}
              </>
            )}
          </CardContent>
        </Card>
      )}

      <OtherPrincipals usage={lookup.byKey} tokens={tokens} />

      <TokenDialogs
        dialog={dialog}
        onClose={() => setDialog(null)}
        onUnauthorized={onUnauthorized}
        onCreated={(result) => setCreated(result)}
        onChanged={() => list.refresh()}
      />
    </div>
  )
}

function Pager({
  page,
  pages,
  total,
  onPage,
}: {
  page: number
  pages: number
  total: number
  onPage: (page: number) => void
}) {
  const from = total === 0 ? 0 : page * PAGE_SIZE + 1
  const to = Math.min(total, (page + 1) * PAGE_SIZE)
  return (
    <div className="ml-auto flex items-center gap-1 text-[11px] text-muted-foreground tabular-nums">
      <span>
        {from.toLocaleString("en-US")}–{to.toLocaleString("en-US")} of {total.toLocaleString("en-US")}
      </span>
      <Button variant="ghost" size="icon" aria-label="Previous page" disabled={page <= 0} onClick={() => onPage(page - 1)}>
        <ChevronLeft />
      </Button>
      <Button
        variant="ghost"
        size="icon"
        aria-label="Next page"
        disabled={page >= pages - 1}
        onClick={() => onPage(page + 1)}
      >
        <ChevronRight />
      </Button>
    </div>
  )
}

/** Create / edit / revoke / delete dialogs, shared by the list and detail views. */
export function TokenDialogs({
  dialog,
  onClose,
  onUnauthorized,
  onCreated,
  onChanged,
  onDeleted,
}: {
  dialog: TokenDialogState
  onClose: () => void
  onUnauthorized: () => void
  onCreated?: (result: CreatedToken) => void
  onChanged: () => void
  onDeleted?: (id: string) => void
}) {
  const handleError = (cause: unknown): string => {
    if (cause instanceof UnauthorizedError) {
      onUnauthorized()
      return "session expired"
    }
    return cause instanceof Error ? cause.message : String(cause)
  }
  if (dialog == null) return null
  if (dialog.kind === "create") {
    return (
      <TokenDialog
        onClose={onClose}
        onSubmit={async (body) => {
          try {
            const result = await createToken({
              name: body.name,
              description: body.description,
              expires_at_ms: body.expires_at_ms ?? null,
            })
            onCreated?.(result)
            onClose()
            onChanged()
            return null
          } catch (cause) {
            return handleError(cause)
          }
        }}
      />
    )
  }
  if (dialog.kind === "edit") {
    return (
      <TokenDialog
        token={dialog.token}
        onClose={onClose}
        onSubmit={async (body) => {
          try {
            await updateToken(dialog.token.id, body)
            onClose()
            onChanged()
            return null
          } catch (cause) {
            return handleError(cause)
          }
        }}
      />
    )
  }
  return (
    <ConfirmDialog
      kind={dialog.kind}
      token={dialog.token}
      onClose={onClose}
      onConfirm={async () => {
        try {
          if (dialog.kind === "revoke") {
            await revokeToken(dialog.token.id)
            onClose()
            onChanged()
          } else {
            await deleteToken(dialog.token.id)
            onClose()
            if (onDeleted) onDeleted(dialog.token.id)
            else onChanged()
          }
          return null
        } catch (cause) {
          return handleError(cause)
        }
      }}
    />
  )
}

function DisabledState() {
  return (
    <Card>
      <CardContent className="flex items-start gap-3 pt-4">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted">
          <ShieldOff className="size-4" />
        </span>
        <div className="flex flex-col gap-1 text-xs">
          <div className="text-sm font-medium">Token management is disabled</div>
          <div className="text-muted-foreground leading-5">
            Set <code className="font-mono text-foreground">RJ_API_TOKENS_MODE=on</code> and{" "}
            <code className="font-mono text-foreground">RJ_API_STATE_DIR</code> on the load balancer
            (it also needs <code className="font-mono text-foreground">RJ_UI_AUTH_TOKEN</code> and{" "}
            <code className="font-mono text-foreground">RJ_UPSTREAM_TOKEN</code>). Usage is still
            accounted as <span className="font-mono">master</span> /{" "}
            <span className="font-mono">anonymous</span> and is visible on the Usage tab.
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

function SecretPanel({ created, onDismiss }: { created: CreatedToken; onDismiss: () => void }) {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(created.secret)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // Clipboard needs a secure context; fall back to selecting the text.
      const node = document.getElementById("rj-new-secret")
      if (node) {
        const range = document.createRange()
        range.selectNodeContents(node)
        const selection = window.getSelection()
        selection?.removeAllRanges()
        selection?.addRange(range)
      }
    }
  }
  return (
    <div
      role="alert"
      className="rounded-lg border p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--status-warning) 45%, transparent)",
        background: "color-mix(in srgb, var(--status-warning) 7%, var(--card))",
      }}
    >
      <div className="flex items-start gap-3">
        <AlertTriangle className="mt-0.5 size-4 shrink-0" style={{ color: "var(--status-warning)" }} />
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <div className="text-sm font-medium">
            Token “{created.token.name}” created — copy it now
          </div>
          <div className="text-muted-foreground text-xs">
            This is the only time the secret is shown. Ramjet stores only its SHA-256 hash; if it is
            lost, revoke this token and mint a new one.
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <code
              id="rj-new-secret"
              className="min-w-0 flex-1 rounded-md border border-border bg-card px-2.5 py-1.5 font-mono text-xs break-all select-all"
            >
              {created.secret}
            </code>
            <Button onClick={() => void copy()} className="min-w-20">
              {copied ? <Check /> : <Copy />}
              {copied ? "copied" : "copy"}
            </Button>
          </div>
          <div className="text-faint-foreground font-mono text-[11px] break-all">
            Authorization: Bearer {created.token.prefix}_…
          </div>
          <div>
            <Button variant="outline" onClick={onDismiss}>
              I have stored it
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

function TokenTable({
  tokens,
  usage,
  now,
  readOnly,
  onEdit,
  onRevoke,
  onDelete,
}: {
  tokens: ApiToken[]
  usage: (key: string) => StatsTotal | "none" | "unknown"
  now: number
  readOnly: boolean
  onEdit: (token: ApiToken) => void
  onRevoke: (token: ApiToken) => void
  onDelete: (token: ApiToken) => void
}) {
  const blank = (value: "none" | "unknown") =>
    value === "none" ? (
      <span className="text-faint-foreground">0</span>
    ) : (
      <span className="text-faint-foreground" title="not among the 500 busiest principals in the last 24 h">
        —
      </span>
    )
  return (
    <div className="overflow-x-auto rounded-md border border-border">
      <table className="w-full text-xs">
        <thead className="bg-muted/60">
          <tr className="text-faint-foreground text-[11px]">
            {[
              ["name", "left"],
              ["token", "left"],
              ["status", "left"],
              ["created", "left"],
              ["expires", "left"],
              ["last used", "left"],
              ["24h req", "right"],
              ["24h tokens", "right"],
              ["24h cache hit", "right"],
              ["", "right"],
            ].map(([label, align], index) => (
              <th
                key={index}
                className={`px-2.5 py-1.5 font-medium whitespace-nowrap ${align === "right" ? "text-right" : "text-left"}`}
              >
                {label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {tokens.map((token) => {
            const status = tokenStatusOf(token, now)
            const stats = usage(token.id)
            const soon =
              status === "active" && token.expires_at_ms != null && token.expires_at_ms - now < 7 * DAY
            return (
              <tr
                key={token.id}
                onClick={() => {
                  window.location.hash = detailHref(token.id)
                }}
                className={`cursor-pointer border-t border-border/60 hover:bg-muted/50 ${status === "active" ? "" : "opacity-70"}`}
              >
                <td className="max-w-[260px] px-2.5 py-1.5">
                  <a
                    href={detailHref(token.id)}
                    onClick={(event) => event.stopPropagation()}
                    className="block truncate font-medium underline-offset-2 hover:underline"
                  >
                    {token.name}
                  </a>
                  {token.description ? (
                    <div className="text-muted-foreground truncate text-[11px]" title={token.description}>
                      {token.description}
                    </div>
                  ) : null}
                </td>
                <td className="px-2.5 py-1.5 font-mono text-[11px] whitespace-nowrap">
                  {token.prefix}
                  <span className="text-faint-foreground">_…</span>
                </td>
                <td className="px-2.5 py-1.5">
                  <StatusBadge status={status} />
                </td>
                <td className="text-muted-foreground px-2.5 py-1.5">
                  <When t={token.created_at_ms} now={now} />
                </td>
                <td className="px-2.5 py-1.5">
                  <ExpiryCell token={token} status={status} soon={soon} now={now} />
                </td>
                <td className="text-muted-foreground px-2.5 py-1.5">
                  <When t={token.last_used_ms} now={now} empty="never" />
                </td>
                <td className="px-2.5 py-1.5 text-right font-mono tabular-nums">
                  {typeof stats === "string" ? blank(stats) : fmtCount(stats.requests)}
                </td>
                <td className="px-2.5 py-1.5 text-right font-mono tabular-nums">
                  {typeof stats === "string" ? blank(stats) : fmtCount(stats.prompt_tokens + stats.completion_tokens)}
                </td>
                <td className="px-2.5 py-1.5 text-right font-mono tabular-nums">
                  {typeof stats !== "string" && stats.cache_hit_ratio != null ? (
                    fmtPct(stats.cache_hit_ratio * 100, 1)
                  ) : (
                    <span className="text-faint-foreground">—</span>
                  )}
                </td>
                <td className="px-1.5 py-1 text-right whitespace-nowrap" onClick={(event) => event.stopPropagation()}>
                  {readOnly ? null : (
                    <TokenActionButtons
                      token={token}
                      status={status}
                      onEdit={onEdit}
                      onRevoke={onRevoke}
                      onDelete={onDelete}
                    />
                  )}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

export function ExpiryCell({
  token,
  status,
  soon,
  now,
}: {
  token: ApiToken
  status: TokenStatus
  soon: boolean
  now: number
}) {
  if (status === "revoked") {
    return (
      <span className="text-muted-foreground whitespace-nowrap" title={fmtDateTime(token.revoked_at_ms)}>
        revoked {fmtRelative(token.revoked_at_ms, now)}
      </span>
    )
  }
  return (
    <span className="inline-flex items-center gap-1" style={soon ? { fontWeight: 600 } : undefined}>
      {soon ? (
        <AlertTriangle
          aria-label="expires within 7 days"
          className="size-3"
          style={{ color: "var(--status-warning)" }}
        />
      ) : null}
      <When t={token.expires_at_ms} now={now} empty="never" />
    </span>
  )
}

export function TokenActionButtons({
  token,
  status,
  onEdit,
  onRevoke,
  onDelete,
  labels = false,
}: {
  token: ApiToken
  status: TokenStatus
  onEdit: (token: ApiToken) => void
  onRevoke: (token: ApiToken) => void
  onDelete: (token: ApiToken) => void
  labels?: boolean
}) {
  const size = labels ? "default" : "icon"
  const variant = labels ? "outline" : "ghost"
  return (
    <>
      <Button variant={variant} size={size} aria-label={`Edit ${token.name}`} title="Edit" onClick={() => onEdit(token)}>
        <Pencil />
        {labels ? "Edit" : null}
      </Button>
      <Button
        variant={variant}
        size={size}
        aria-label={`Revoke ${token.name}`}
        title="Revoke"
        disabled={status === "revoked"}
        onClick={() => onRevoke(token)}
      >
        <Ban />
        {labels ? "Revoke" : null}
      </Button>
      <Button variant={variant} size={size} aria-label={`Delete ${token.name}`} title="Delete" onClick={() => onDelete(token)}>
        <Trash2 />
        {labels ? "Delete" : null}
      </Button>
    </>
  )
}

/** Master / anonymous / rejected / deleted tokens over the same 24 h. */
function OtherPrincipals({ usage, tokens }: { usage: Map<string, StatsTotal>; tokens: ApiToken[] }) {
  const ids = useMemo(() => new Set(tokens.map((token) => token.id)), [tokens])
  const rows = [...usage.values()].filter(
    (total) => !ids.has(total.key) && total.key !== SERVER_OTHER_KEY && total.requests > 0,
  )
  if (rows.length === 0) return null
  return (
    <div className="text-muted-foreground flex flex-wrap gap-x-4 gap-y-1 px-1 text-[11px]">
      <span className="text-faint-foreground">other principals, last 24 h:</span>
      {rows.map((total) => (
        <a key={total.key} href={detailHref(total.key)} className="font-mono underline-offset-2 hover:underline">
          {total.key} {fmtCount(total.requests)} req
          {total.key === "rejected" ? " (401)" : ""}
        </a>
      ))}
    </div>
  )
}

// --------------------------------------------------------------- dialogs

type ExpiryPreset = "keep" | "never" | "1d" | "7d" | "30d" | "90d" | "custom"

const PRESET_DAYS: Partial<Record<ExpiryPreset, number>> = { "1d": 1, "7d": 7, "30d": 30, "90d": 90 }

/** `datetime-local` value for a local time. */
function toLocalInput(t: number): string {
  const date = new Date(t - new Date(t).getTimezoneOffset() * 60_000)
  return date.toISOString().slice(0, 16)
}

function TokenDialog({
  token,
  onClose,
  onSubmit,
}: {
  token?: ApiToken
  onClose: () => void
  onSubmit: (body: { name: string; description: string; expires_at_ms?: number | null }) => Promise<string | null>
}) {
  const editing = token != null
  const [name, setName] = useState(token?.name ?? "")
  const [description, setDescription] = useState(token?.description ?? "")
  const [preset, setPreset] = useState<ExpiryPreset>(editing ? "keep" : "90d")
  const [custom, setCustom] = useState(() => toLocalInput(Date.now() + 30 * DAY))
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const expiry = (): { ok: true; value: number | null | undefined } | { ok: false; error: string } => {
    if (preset === "keep") return { ok: true, value: undefined }
    if (preset === "never") return { ok: true, value: null }
    const days = PRESET_DAYS[preset]
    if (days != null) return { ok: true, value: Date.now() + days * DAY }
    const t = new Date(custom).getTime()
    if (!Number.isFinite(t)) return { ok: false, error: "choose a valid expiry date" }
    if (t <= Date.now()) return { ok: false, error: "expiry must be in the future" }
    return { ok: true, value: t }
  }
  const resolved = expiry()
  const trimmed = name.trim()
  const valid = trimmed.length >= 1 && trimmed.length <= 64 && description.length <= 512 && resolved.ok

  const submit = async () => {
    if (!valid || busy || !resolved.ok) return
    setBusy(true)
    setError(null)
    const body: { name: string; description: string; expires_at_ms?: number | null } = {
      name: trimmed,
      description,
    }
    if (resolved.value !== undefined) body.expires_at_ms = resolved.value
    const failure = await onSubmit(body)
    setBusy(false)
    if (failure) setError(failure)
  }

  const presets: Array<{ value: ExpiryPreset; label: string }> = [
    ...(editing ? [{ value: "keep" as const, label: "keep" }] : []),
    { value: "never", label: "never" },
    { value: "1d", label: "1 day" },
    { value: "7d", label: "7 days" },
    { value: "30d", label: "30 days" },
    { value: "90d", label: "90 days" },
    { value: "custom", label: "custom" },
  ]

  return (
    <Modal
      title={editing ? `Edit “${token.name}”` : "New API token"}
      description={
        editing
          ? "Renaming or re-describing a token never changes its secret."
          : "The secret is shown once after creation. Clients send it as a Bearer token or x-api-key."
      }
      icon={<KeyRound className="size-4" />}
      busy={busy}
      onClose={onClose}
      footer={
        <>
          <Button variant="outline" disabled={busy} onClick={onClose}>
            Cancel
          </Button>
          <Button disabled={!valid || busy} onClick={() => void submit()} className="min-w-28">
            {busy ? "Saving…" : editing ? "Save changes" : "Create token"}
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-3"
        onSubmit={(event) => {
          event.preventDefault()
          void submit()
        }}
      >
        <label className="flex flex-col gap-1 text-[11px] text-muted-foreground">
          name
          <input
            className={inputClass}
            value={name}
            maxLength={64}
            placeholder="helix-prod"
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1 text-[11px] text-muted-foreground">
          <span className="flex justify-between">
            description <span className="text-faint-foreground">{description.length}/512</span>
          </span>
          <textarea
            className={`${inputClass} min-h-16 resize-y`}
            value={description}
            maxLength={512}
            placeholder="Who uses this token, and for what"
            onChange={(event) => setDescription(event.target.value)}
          />
        </label>
        <div className="flex flex-col gap-1.5 text-[11px] text-muted-foreground">
          <span>
            expires
            {editing ? (
              <span className="text-faint-foreground">
                {" "}
                · currently{" "}
                {token.expires_at_ms == null ? "never" : `${fmtRelative(token.expires_at_ms)} (${fmtDateTime(token.expires_at_ms)})`}
              </span>
            ) : null}
          </span>
          <div role="radiogroup" aria-label="Expiry" className="bg-muted flex flex-wrap items-center gap-0.5 self-start rounded-lg p-0.5">
            {presets.map((option) => (
              <Button
                key={option.value}
                type="button"
                variant="segment"
                role="radio"
                aria-checked={option.value === preset}
                data-active={option.value === preset}
                onClick={() => setPreset(option.value)}
              >
                {option.label}
              </Button>
            ))}
          </div>
          {preset === "custom" ? (
            <input
              type="datetime-local"
              className={`${inputClass} max-w-[240px]`}
              value={custom}
              min={toLocalInput(Date.now() + 60_000)}
              onChange={(event) => setCustom(event.target.value)}
            />
          ) : null}
          <span className="text-faint-foreground">
            {resolved.ok
              ? resolved.value === undefined
                ? "expiry unchanged"
                : resolved.value === null
                  ? "never expires — revoke it when no longer needed"
                  : `expires ${fmtDateTime(resolved.value)}`
              : resolved.error}
          </span>
        </div>
        {error ? (
          <div
            className="rounded-md border px-3 py-2 text-xs"
            style={{
              borderColor: "color-mix(in srgb, var(--status-critical) 35%, transparent)",
              color: "var(--status-critical)",
            }}
          >
            {error}
          </div>
        ) : null}
        <button type="submit" hidden />
      </form>
    </Modal>
  )
}

function ConfirmDialog({
  kind,
  token,
  onClose,
  onConfirm,
}: {
  kind: "revoke" | "delete"
  token: ApiToken
  onClose: () => void
  onConfirm: () => Promise<string | null>
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const confirm = async () => {
    setBusy(true)
    setError(null)
    const failure = await onConfirm()
    setBusy(false)
    if (failure) setError(failure)
  }
  return (
    <Modal
      title={kind === "revoke" ? `Revoke “${token.name}”?` : `Delete “${token.name}”?`}
      description={
        kind === "revoke"
          ? "Requests carrying this token are rejected with 401 immediately. The token stays listed as revoked, with its usage history."
          : "The token is removed from the list and can no longer authenticate. Its usage history is kept under its id and stays visible on the Usage tab."
      }
      icon={kind === "revoke" ? <Ban className="size-4" /> : <Trash2 className="size-4" />}
      busy={busy}
      onClose={onClose}
      width={460}
      footer={
        <>
          <Button variant="outline" disabled={busy} onClick={onClose} data-autofocus>
            Cancel
          </Button>
          <Button
            disabled={busy}
            onClick={() => void confirm()}
            className="min-w-24 text-white hover:opacity-90"
            style={{ background: "var(--status-critical)" }}
          >
            {busy ? "Working…" : kind === "revoke" ? "Revoke token" : "Delete token"}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-2">
        <div className="rounded-md border border-border bg-muted/40 px-3 py-2 font-mono text-xs">
          {token.prefix}_…
        </div>
        {error ? (
          <div className="text-xs" style={{ color: "var(--status-critical)" }}>
            {error}
          </div>
        ) : null}
      </div>
    </Modal>
  )
}
