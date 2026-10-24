export class Lib {
  static isArray(x: unknown): boolean { return false; }
}

export function helperFn(): number { return 1; }

export function checkList(xs: unknown) {
  return Array.isArray(xs);
}

export function runLocal(helperFn: () => number) {
  return helperFn();
}

function escapeIt(s: string): string { return s; }
function formatA(): void {}
function formatB(): void {}

export const TABLE = { a: [1, formatA], b: formatB };

export function label(topic: string) {
  return `[${escapeIt(topic)}]`;
}

export class Poller {
  start(every: (f: () => void) => void) { every(this.poll.bind(this)); }
  private poll() {}
}
