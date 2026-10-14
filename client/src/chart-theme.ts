/* ---------------------------------------------------------------------------
   ECharts 主题取值：canvas 渲染拿不到 CSS 变量，只能在运行时把 @theme token
   读成字符串再传进 option，保证图表配色和页面（style.css 里的设计 token）一致。
--------------------------------------------------------------------------- */

export function cssVar(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export interface ChartTheme {
  ink: string; ink2: string; ink3: string; line: string; surface: string;
  brand: string; brandSoft: string; teal: string;
  ok: string; warn: string; err: string; info: string;
  mono: string;
}

export function chartTheme(): ChartTheme {
  return {
    ink: cssVar("--color-ink", "#17171a"),
    ink2: cssVar("--color-ink-2", "#52525b"),
    ink3: cssVar("--color-ink-3", "#8e8e97"),
    line: cssVar("--color-line", "#e7e7ea"),
    surface: cssVar("--color-surface", "#ffffff"),
    brand: cssVar("--color-brand-500", "#f97316"),
    brandSoft: cssVar("--color-brand-200", "#fed7aa"),
    teal: cssVar("--color-teal-500", "#14b8a6"),
    ok: cssVar("--color-ok-500", "#10b981"),
    warn: cssVar("--color-warn-500", "#f59e0b"),
    err: cssVar("--color-err-500", "#ef4444"),
    info: "#60a5fa",
    mono: cssVar("--font-mono", "Consolas, monospace"),
  };
}

/** 饼图 / 多序列共用的分类配色（品牌橙打头，后面依次是语义色与中性色）。 */
export function chartPalette(t: ChartTheme): string[] {
  return [t.brand, t.teal, t.ok, t.warn, t.info, t.brandSoft, "#a78bfa", t.ink3];
}
