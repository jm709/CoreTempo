export function isAtBottom(
  offset: number,
  viewport: number,
  scrollSize: number,
  slackPx = 8,
): boolean {
  return offset + viewport >= scrollSize - slackPx;
}
