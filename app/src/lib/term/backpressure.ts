export const HIGH_WATER = 1_048_576; // ~1 MB unparsed per terminal (contracts §10)
export const LOW_WATER = 262_144;

/// Tracks bytes handed to term.write but not yet parsed (write-callback pending).
/// Crossing HIGH_WATER pauses core-side PTY reads; draining under LOW_WATER resumes.
export class Backpressure {
  private outstanding = 0;
  private paused = false;

  constructor(
    private readonly onPause: (paused: boolean) => void,
    private readonly high: number = HIGH_WATER,
    private readonly low: number = LOW_WATER,
  ) {}

  wrote(n: number): void {
    this.outstanding += n;
    if (!this.paused && this.outstanding > this.high) {
      this.paused = true;
      this.onPause(true);
    }
  }

  parsed(n: number): void {
    this.outstanding = Math.max(0, this.outstanding - n);
    if (this.paused && this.outstanding < this.low) {
      this.paused = false;
      this.onPause(false);
    }
  }

  get bytesOutstanding(): number {
    return this.outstanding;
  }
}
