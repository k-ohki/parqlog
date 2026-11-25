// 系列の色（色覚の違いでも区別しやすい順）
export const PALETTE = [
  "#2563eb", "#e8590c", "#0f9d58", "#c2255c", "#7048e8",
  "#0b7285", "#b08900", "#5c7cfa", "#d6336c", "#2b8a3e",
];

export function color(i: number): string {
  return PALETTE[i % PALETTE.length];
}

export function alpha(hex: string, a: number): string {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}
