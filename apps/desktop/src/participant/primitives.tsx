import { useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { arrivalAgeText, formatGainDb, sourcePlayoutLabel } from './labels.ts'
import type { EndpointView, OutputDeviceView } from './types.ts'

export type IconName =
  | 'wave'
  | 'mic'
  | 'headphones'
  | 'users'
  | 'refresh'
  | 'alert'
  | 'speaker'

const iconPaths: Record<IconName, ReactNode> = {
  wave: <path d="M3 12h3l2-7 4 14 3-10 2 6h4" />,
  mic: (
    <>
      <rect x="9" y="3" width="6" height="12" rx="3" />
      <path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21M9 21h6" />
    </>
  ),
  headphones: (
    <>
      <path d="M4 14v-2a8 8 0 0 1 16 0v2" />
      <path d="M4 14a2 2 0 0 1 2-2h1v7H6a2 2 0 0 1-2-2v-3ZM20 14a2 2 0 0 0-2-2h-1v7h1a2 2 0 0 0 2-2v-3Z" />
    </>
  ),
  users: (
    <>
      <path d="M16 20v-1.5a4.5 4.5 0 0 0-4.5-4.5h-4A4.5 4.5 0 0 0 3 18.5V20" />
      <circle cx="9.5" cy="7" r="4" />
      <path d="M17 11a3.5 3.5 0 0 0 0-7M21 20v-1.5a4.5 4.5 0 0 0-2.7-4.1" />
    </>
  ),
  refresh: (
    <>
      <path d="M20 6v5h-5" />
      <path d="M18 16a8 8 0 1 1 1.5-8.5L20 11" />
    </>
  ),
  alert: (
    <>
      <path d="M12 3 2.8 20h18.4L12 3Z" />
      <path d="M12 9v5M12 17.5h.01" />
    </>
  ),
  speaker: (
    <>
      <path d="M4 9v6h4l5 4V5L8 9H4" />
      <path d="M16 10a3 3 0 0 1 0 4" />
    </>
  ),
}

function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value))
}

export function Icon({ name, size = 18 }: { name: IconName; size?: number }) {
  return (
    <svg
      aria-hidden="true"
      className="icon"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {iconPaths[name]}
    </svg>
  )
}

export function StatusDot({
  tone,
  pulse = false,
}: {
  tone: 'positive' | 'warning' | 'danger' | 'info' | 'neutral'
  pulse?: boolean
}) {
  return (
    <span
      aria-hidden="true"
      className={`status-dot status-dot--${tone}${pulse ? ' status-dot--pulse' : ''}`}
    />
  )
}

export function FieldError({
  id,
  message,
}: {
  id: string
  message: string | null
}) {
  if (message === null || message === '') {
    return null
  }
  return (
    <p className="field-error" id={id} role="alert">
      {message}
    </p>
  )
}

export function ToggleField({
  id,
  label,
  checked,
  mixed,
  emphasis,
  disabled,
  describedBy,
  onCommit,
}: {
  id: string
  label: string
  checked: boolean
  /** Present on group controls. A partly-on group turns fully on when clicked. */
  mixed?: boolean
  emphasis?: 'primary' | 'secondary'
  disabled: boolean
  describedBy?: string
  onCommit: (checked: boolean) => void
}) {
  const partly = mixed === true
  const toggleClass = partly ? 'toggle toggle--mixed' : checked ? 'toggle toggle--on' : 'toggle'
  return (
    <div className={emphasis ? `choice-row choice-row--${emphasis}` : 'choice-row'}>
      <span id={`${id}-label`}>{label}</span>
      <button
        type="button"
        className={toggleClass}
        role={mixed === undefined ? 'switch' : 'checkbox'}
        aria-checked={partly ? 'mixed' : checked}
        aria-labelledby={`${id}-label`}
        aria-describedby={describedBy}
        disabled={disabled}
        onClick={() => {
          if (!disabled) {
            onCommit(partly ? true : !checked)
          }
        }}
      >
        <span className="toggle__thumb" />
      </button>
    </div>
  )
}

export function SliderField({
  id,
  label,
  value,
  mixed = false,
  min,
  max,
  step,
  disabled,
  format,
  onCommit,
}: {
  id: string
  label: string
  value: number
  /** The neutral slider position is only a starting point when devices differ. */
  mixed?: boolean
  min: number
  max: number
  step: number
  disabled: boolean
  format: (value: number) => string
  onCommit: (value: number) => void
}) {
  const pointerActive = useRef(false)
  const [draft, setDraft] = useState<number | null>(null)
  const shown = draft ?? value
  const shownText = mixed && draft === null ? '各设备不同，调整后统一' : format(shown)

  function endPointer(next: number) {
    if (!pointerActive.current) return
    pointerActive.current = false
    setDraft(null)
    if (Number.isFinite(next)) onCommit(next)
  }

  return (
    <label className="range-control" htmlFor={id}>
      <span className="field__label">{label}</span>
      <input
        id={id}
        type="range"
        min={min}
        max={max}
        step={step}
        disabled={disabled}
        value={shown}
        aria-valuetext={shownText}
        onPointerDown={(event) => {
          if (disabled) return
          pointerActive.current = true
          event.currentTarget.setPointerCapture(event.pointerId)
        }}
        onChange={(event) => {
          const next = event.currentTarget.valueAsNumber
          if (pointerActive.current) {
            setDraft(next)
            return
          }
          if (!Number.isFinite(next)) return
          onCommit(next)
          setDraft(null)
        }}
        onPointerUp={(event) => {
          endPointer(event.currentTarget.valueAsNumber)
        }}
        onLostPointerCapture={(event) => {
          endPointer(event.currentTarget.valueAsNumber)
        }}
        onBlur={() => {
          pointerActive.current = false
          setDraft(null)
        }}
      />
      <output htmlFor={id}>{shownText}</output>
    </label>
  )
}

export function SelectField({
  id,
  label,
  value,
  disabled,
  describedBy,
  onCommit,
  children,
}: {
  id: string
  label: string
  value: string
  disabled: boolean
  describedBy?: string
  onCommit: (value: string) => void
  children: ReactNode
}) {
  return (
    <div className="field">
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      <select
        id={id}
        value={value}
        disabled={disabled}
        aria-describedby={describedBy}
        onChange={(event) => {
          const next = event.currentTarget.value
          if (next !== '') {
            onCommit(next)
          }
        }}
      >
        {children}
      </select>
    </div>
  )
}

export function MeterReadout({
  label,
  peakDb,
  overloaded = false,
}: {
  label: string
  peakDb: number | null
  overloaded?: boolean
}) {
  const text = peakDb === null ? '无测量' : formatGainDb(peakDb)
  const width =
    peakDb === null ? 0 : clamp(((peakDb + 60) / 60) * 100, 0, 100)

  return (
    <div>
      <div
        role="meter"
        aria-label={label}
        aria-valuetext={text}
        aria-valuemin={peakDb === null ? undefined : -60}
        aria-valuemax={peakDb === null ? undefined : 0}
        aria-valuenow={peakDb === null ? undefined : clamp(peakDb, -60, 0)}
        className={overloaded ? 'meter meter--overload' : 'meter'}
      >
        <span className="meter__fill" style={{ width: `${width}%` }} />
      </div>
      <p className="readout">{label}：{text}</p>
    </div>
  )
}

export function DetailsBlock({
  summary,
  children,
}: {
  summary: string
  children: ReactNode
}) {
  return (
    <details className="details">
      <summary>{summary}</summary>
      <div className="details__body">{children}</div>
    </details>
  )
}

/** Actual renderer observations for one source, independently per output. */
export function SourcePlaybackReadout({ outputs, endpoints, memberId, sourceId }: {
  outputs: readonly OutputDeviceView[]
  endpoints: readonly EndpointView[]
  memberId: string | null
  sourceId: string
}) {
  const rows = outputs.flatMap((output) => {
    const matches = (entry: { memberId: string | null; sourceId: string }) =>
      entry.memberId === memberId && entry.sourceId === sourceId
    const playback = output.sources?.find(matches)
    const error = output.sourceErrors?.find(matches)
    return playback === undefined && error === undefined ? [] : [{ output, playback, error }]
  })
  if (rows.length === 0) return null
  return (
    <ul className="source-playout-list">
      {rows.map(({ output, playback, error }) => (
        <li key={output.endpointId}>
          <span>{endpoints.find((endpoint) => endpoint.id === output.endpointId)?.name ?? output.endpointId}：
            {playback ? sourcePlayoutLabel(playback.phase) : '此路输出发生问题'}</span>
          {playback ? <p className="readout">{arrivalAgeText(playback.lastArrivalAgeMs)}</p> : null}
          {error ? <p className="field-error">{error.detail}</p> : null}
          {playback?.lastError && playback.lastError !== error?.detail
            ? <p className="field-error">{playback.lastError}</p> : null}
        </li>
      ))}
    </ul>
  )
}

export function EmptyState({ children }: { children: ReactNode }) {
  return <p className="empty-state">{children}</p>
}
