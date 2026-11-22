// Charts for the admin metrics page (Rust templates/admin/metrics.html). The LiveView puts the
// series in data-charts (an escaped attribute, so hostnames can't break out of it); the element
// is phx-update="ignore" with the range in its id, so a range change remounts it and the charts are rebuilt here.
import Chart from "../vendor/chart.umd.min.js"

// Chart.js parses colors for hover shades and doesn't know oklch, so theme colors go through a
// canvas to come out as rgb
const probe = document.createElement("canvas").getContext("2d", {willReadFrequently: true})
const rgb = value => {
  probe.clearRect(0, 0, 1, 1)
  probe.fillStyle = value
  probe.fillRect(0, 0, 1, 1)
  const [r, g, b, a] = probe.getImageData(0, 0, 1, 1).data
  return `rgba(${r}, ${g}, ${b}, ${(a / 255).toFixed(3)})`
}
const cssVar = (el, name, fallback) =>
  rgb(getComputedStyle(el).getPropertyValue(name).trim() || fallback)

const SERIES = {
  light: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"],
  dark: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
}
const STATUS = {good: "#0ca30c", warning: "#fab219", critical: "#d03b3b"}

const dur = s => {
  if (s == null) return "-"
  if (s < 60) return `${+s.toFixed(1)}s`
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`
  return `${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m`
}
const durTick = s => {
  if (s < 60) return `${s}s`
  if (s < 3600) return s % 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s / 60}m`
  return s % 3600 ? `${Math.floor(s / 3600)}h ${(s % 3600) / 60}m` : `${s / 3600}h`
}
// ticks step by round durations (Chart.js would pick round numbers of seconds)
const STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 14400, 28800]
const durSteps = {
  afterDataLimits: scale => {
    const step = STEPS.find(x => scale.max / x <= 6) ?? STEPS[STEPS.length - 1]
    scale.min = 0
    scale.max = Math.max(step, Math.ceil(scale.max / step) * step)
    scale.durStep = step
  },
  afterBuildTicks: scale => {
    scale.ticks = []
    for (let v = 0; v <= scale.max; v += scale.durStep) scale.ticks.push({value: v})
  },
}

function build(el, data) {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches ||
    document.documentElement.dataset.theme === "dark"
  const series = SERIES[dark ? "dark" : "light"]
  const surface = cssVar(el, "--color-base-200", "#f2f2f2")
  const text = rgb(getComputedStyle(el).color)
  const grid = cssVar(el, "--color-base-300", "#e5e5e5")

  Chart.defaults.font.family = getComputedStyle(document.body).fontFamily
  Chart.defaults.color = text
  Chart.defaults.borderColor = grid

  const legend = {position: "bottom", labels: {boxWidth: 12, boxHeight: 12}}
  const lineLegend = {position: "bottom", labels: {boxWidth: 16, boxHeight: 2}}
  const axis = {grid: {color: grid, drawTicks: false}, border: {display: false}, ticks: {padding: 6}}
  const timeAxis = {...axis, grid: {display: false}, ticks: {...axis.ticks, maxRotation: 0, autoSkipPadding: 16}}
  const durAxis = {...axis, ...durSteps, beginAtZero: true, ticks: {...axis.ticks, callback: durTick}}
  const durTooltip = {callbacks: {label: c => ` ${dur(c.parsed.y ?? c.parsed.x)}  ${c.dataset.label}`}}
  const crosshair = {mode: "index", intersect: false}
  const line = (label, values, color) => ({
    label, data: values, borderColor: color, backgroundColor: color, borderWidth: 2,
    pointRadius: 0, pointHoverRadius: 5, pointHitRadius: 12, spanGaps: true, tension: 0,
  })
  const canvas = name => el.querySelector(`canvas[data-chart="${name}"]`)
  const charts = []

  const outcomes = canvas("outcomes")
  if (outcomes) {
    const bar = (label, values, color) => ({
      label, data: values, backgroundColor: color, hoverBackgroundColor: color,
      borderColor: surface, borderWidth: {top: 2}, borderSkipped: "bottom", maxBarThickness: 32,
    })
    charts.push(new Chart(outcomes, {
      type: "bar",
      data: {
        labels: data.buckets,
        datasets: [
          bar("Completed", data.outcomes.completed, STATUS.good),
          bar("Failed or lost", data.outcomes.failed, STATUS.critical),
          bar("Rejected (agent full)", data.outcomes.rejected, STATUS.warning),
          bar("Stopped by us", data.outcomes.other, "rgba(128, 128, 128, 0.6)"),
        ],
      },
      options: {
        maintainAspectRatio: false, interaction: crosshair, plugins: {legend},
        scales: {x: {...timeAxis, stacked: true}, y: {...axis, stacked: true, beginAtZero: true, ticks: {...axis.ticks, precision: 0}}},
      },
    }))
  }

  const provision = canvas("provision")
  if (provision) {
    // color follows the agent, not its rank, so another range doesn't repaint the ones that stay
    const order = data.provision.map(s => s.label).sort()
    charts.push(new Chart(provision, {
      type: "line",
      data: {labels: data.buckets, datasets: data.provision.map(s => line(s.label, s.data, series[order.indexOf(s.label) % series.length]))},
      options: {maintainAspectRatio: false, interaction: crosshair, plugins: {legend: lineLegend, tooltip: durTooltip}, scales: {x: timeAxis, y: durAxis}},
    }))
  }

  const phases = canvas("phases")
  if (phases) {
    const step = (label, values, color) => ({
      label, data: values, backgroundColor: color, hoverBackgroundColor: color,
      borderColor: surface, borderWidth: {right: 2}, borderSkipped: "left", maxBarThickness: 20,
    })
    charts.push(new Chart(phases, {
      type: "bar",
      data: {labels: data.phases.agents, datasets: data.phases.steps.map((s, i) => step(s.label, s.data, series[i % series.length]))},
      options: {
        indexAxis: "y", maintainAspectRatio: false,
        plugins: {legend, tooltip: {callbacks: {label: c => ` ${dur(c.parsed.x)}  ${c.dataset.label}`}}},
        scales: {x: {...durAxis, stacked: true}, y: {...axis, stacked: true, grid: {display: false}}},
      },
    }))
  }

  const wait = canvas("wait")
  if (wait) {
    charts.push(new Chart(wait, {
      type: "line",
      data: {labels: data.buckets, datasets: [line("p50", data.wait_p50, series[0]), line("p95", data.wait_p95, series[1])]},
      options: {maintainAspectRatio: false, interaction: crosshair, plugins: {legend: lineLegend, tooltip: durTooltip}, scales: {x: timeAxis, y: durAxis}},
    }))
  }
  return charts
}

export const MetricsCharts = {
  mounted() {
    this.media = window.matchMedia("(prefers-color-scheme: dark)")
    this.rebuild = () => setTimeout(() => this.render())
    this.media.addEventListener("change", this.rebuild)
    this.render()
  },
  render() {
    (this.charts || []).forEach(c => c.destroy())
    this.charts = build(this.el, JSON.parse(this.el.dataset.charts))
  },
  destroyed() {
    this.media.removeEventListener("change", this.rebuild);
    (this.charts || []).forEach(c => c.destroy())
  },
}
