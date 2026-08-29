import { useState, type FormEvent } from "react"
import { ArrowRight, KeyRound, ShieldCheck } from "lucide-react"
import { Button } from "@/components/ui/button"
import { loginUi } from "@/lib/api"

export function LoginScreen({ onAuthenticated }: { onAuthenticated: () => void }) {
  const [token, setToken] = useState("")
  const [working, setWorking] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    if (!token || working) return
    setWorking(true)
    try {
      const session = await loginUi(token)
      if (!session.authenticated) throw new Error("Login was not accepted")
      setToken("")
      setError(null)
      onAuthenticated()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setWorking(false)
    }
  }

  return (
    <main className="relative flex min-h-screen items-center justify-center overflow-hidden px-5 py-12">
      <div aria-hidden className="pointer-events-none absolute inset-0 opacity-70">
        <svg viewBox="0 0 1200 800" className="h-full w-full" preserveAspectRatio="xMidYMid slice">
          <defs>
            <radialGradient id="login-glow" cx="50%" cy="50%" r="50%">
              <stop offset="0" stopColor="#00d5ff" stopOpacity=".2" />
              <stop offset=".55" stopColor="#00d5ff" stopOpacity=".035" />
              <stop offset="1" stopColor="#00d5ff" stopOpacity="0" />
            </radialGradient>
            <pattern id="login-grid" width="48" height="48" patternUnits="userSpaceOnUse">
              <path d="M48 0H0V48" fill="none" stroke="currentColor" strokeOpacity=".07" />
            </pattern>
          </defs>
          <rect width="1200" height="800" fill="url(#login-grid)" />
          <circle cx="600" cy="390" r="440" fill="url(#login-glow)" />
          {[0, 1, 2].map((lane) => (
            <path key={lane} d={`M80 ${330 + lane * 56} C340 ${270 + lane * 60}, 780 ${470 - lane * 45}, 1120 ${340 + lane * 42}`} fill="none" stroke="#00d5ff" strokeOpacity={0.08 + lane * 0.025} strokeWidth="2" strokeDasharray="7 18" />
          ))}
        </svg>
      </div>

      <section className="relative w-full max-w-[430px] rounded-2xl border border-border bg-card/95 p-7 shadow-2xl backdrop-blur-xl md:p-9">
        <div className="mb-7 flex items-center justify-between">
          <div className="flex items-center gap-3">
            <span className="flex size-10 items-center justify-center rounded-xl border border-primary/20 bg-primary/10">
              <svg viewBox="0 0 24 24" className="size-6" fill="none" stroke="#00d5ff" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <path d="M4 5.5 10.5 12 4 18.5M13 5.5 19.5 12 13 18.5" />
              </svg>
            </span>
            <div>
              <div className="wordmark text-xl">ramjet</div>
              <div className="text-[10px] uppercase tracking-[0.22em] text-muted-foreground">control plane</div>
            </div>
          </div>
          <ShieldCheck className="size-5 text-primary" />
        </div>

        <h1 className="text-2xl font-semibold tracking-tight">Welcome back</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          Authenticate to view system load and manage engine topology.
        </p>

        <form onSubmit={(event) => void submit(event)} className="mt-7 flex flex-col gap-4">
          <label className="flex flex-col gap-2 text-xs font-medium">
            Control token
            <span className="relative">
              <KeyRound aria-hidden className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
              <input
                autoFocus
                type="password"
                autoComplete="current-password"
                value={token}
                onChange={(event) => setToken(event.target.value)}
                placeholder="Enter your Ramjet token"
                className="h-11 w-full rounded-lg border border-border bg-background pl-10 pr-3 font-mono text-sm outline-none transition focus:border-primary/60 focus:ring-2 focus:ring-ring/20"
              />
            </span>
          </label>
          {error ? <div className="rounded-lg border border-red-500/30 bg-red-500/5 px-3 py-2 text-xs text-red-500">{error}</div> : null}
          <Button type="submit" disabled={!token || working} className="h-10 text-sm">
            {working ? "Authenticating…" : "Enter control plane"}
            {!working ? <ArrowRight /> : null}
          </Button>
        </form>

        <p className="mt-5 text-center text-[11px] leading-5 text-muted-foreground">
          A signed session is stored as an HttpOnly browser cookie. The token is not retained by this page.
        </p>
      </section>
    </main>
  )
}
