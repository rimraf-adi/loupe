/** Configuration for the app. */
export interface Config {
  port: number;
  run(): void;
}

export type Handler = (req: string) => string;

export enum Level {
  Debug,
  Info,
}

// Not exported
function internalHelper(x: number): number {
  return x * 2;
}

/** Adds numbers. */
export function add(a: number, b: number): number {
  return a + b;
}

export const double = (n: number): number => n * 2;

const hidden = (s: string) => {
  return s;
};

export class Server {
  private secret = 1;

  constructor(private readonly config: Config) {}

  /** Start listening. */
  public start(): void {}

  protected stop(): void {}

  private reset(): void {}

  static create(): Server {
    return new Server({ port: 1, run() {} });
  }
}

export abstract class Base {
  abstract name(): string;
}
