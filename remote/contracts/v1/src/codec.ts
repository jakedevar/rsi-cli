import { validateNamed } from './semantic.js';
export class InvalidWire extends Error { constructor() { super('invalid Remote V1 wire value'); } }
export function check(ok: unknown): asserts ok { if (!ok) throw new InvalidWire(); }
export type Decoder<T> = (value: unknown) => T;
export type Infer<D> = D extends Decoder<infer T> ? T : never;
export const bytes = (s: string): number => new TextEncoder().encode(s).length;
export function text(max: number, nonempty = true): Decoder<string> {
  return v => { check(typeof v === 'string' && v.isWellFormed() && bytes(v) <= max && (!nonempty || v.length > 0)); return v; };
}
export const boolean: Decoder<boolean> = v => { check(typeof v === 'boolean'); return v; };
export const integer = (min: number, max: number): Decoder<number> => v => {
  check(typeof v === 'number' && Number.isSafeInteger(v) && !Object.is(v, -0) && v >= min && v <= max); return v;
};
export const literal = <const T extends string>(s: T): Decoder<T> => v => { check(v === s); return s; };
export const oneOf = <const T extends readonly string[]>(values: T): Decoder<T[number]> => v => {
  check(typeof v === 'string' && values.includes(v)); return v;
};
export const nullable = <T>(d: Decoder<T>): Decoder<T | null> => v => v === null ? null : d(v);
export const defaulted = <T>(d: Decoder<T>, fallback: T): Decoder<T> => v => d(v === undefined ? fallback : v);
export const array = <T>(d: Decoder<T>): Decoder<T[]> => v => { check(Array.isArray(v)); return v.map(d); };
export function object<const S extends Record<string, Decoder<unknown>>>(shape: S): Decoder<{ [K in keyof S]: Infer<S[K]> }> {
  return v => {
    check(v !== null && typeof v === 'object' && !Array.isArray(v));
    const input = v as Record<string, unknown>;
    check(Object.keys(input).every(k => Object.hasOwn(shape, k)));
    const output: Record<string, unknown> = {};
    for (const [k, d] of Object.entries(shape)) output[k] = d(Object.hasOwn(input, k) ? input[k] : undefined);
    // Every property above was constructed by its decoder; this cast connects
    // the runtime key loop to the mapped type, never admits an unchecked value.
    return output as { [K in keyof S]: Infer<S[K]> };
  };
}
export const union = <const D extends readonly Decoder<unknown>[]>(ds: D): Decoder<Infer<D[number]>> => v => {
  for (const d of ds) { try { return d(v) as Infer<D[number]>; } catch (e) { if (!(e instanceof InvalidWire)) throw e; } }
  throw new InvalidWire();
};
export const define = <T>(name: string, d: Decoder<T>): Decoder<T> => v => { const decoded = d(v); validateNamed(name, decoded); return decoded; };
export const DecimalU64: Decoder<string> = v => { const s = text(20)(v); check(/^(0|[1-9][0-9]*)$/.test(s) && BigInt(s) <= 18446744073709551615n); return s; };
export const DecimalI64: Decoder<string> = v => { const s = text(20)(v); check(/^(0|-?[1-9][0-9]*)$/.test(s) && BigInt(s) >= -9223372036854775808n && BigInt(s) <= 9223372036854775807n); return s; };
export const WireUuid: Decoder<string> = v => { const s = text(36)(v); check(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(s)); return s; };
export const Timestamp: Decoder<string> = v => {
  const s = text(35)(v);
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})\.(\d{9})(Z|[+-]\d{2}:\d{2})$/.exec(s);
  check(m);
  const [y, month, day, h, minute, sec] = m.slice(1, 7).map(Number) as [number, number, number, number, number, number];
  const leap = y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0);
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  check(month >= 1 && month <= 12 && day >= 1 && day <= days[month - 1]! && h < 24 && minute < 60 && sec <= 60);
  const zone = m[8]!;
  check(zone === 'Z' || (Number(zone.slice(1, 3)) < 24 && Number(zone.slice(4)) < 60));
  return s;
};
export const OpaqueToken: Decoder<string> = v => { const s = text(2048)(v); check(/^[A-Za-z0-9_-]+$/.test(s)); return s; };
export const DecisionId: Decoder<string> = v => { const s = text(160)(v); identity(s); return s; };
export function identity(s: string): readonly [string, string] {
  const [kind, id, gen, mirror, extra] = s.split(':');
  WireUuid(id);
  if (kind === 'question-slot') {
    check(extra === undefined && (mirror === 'tracked' || mirror === 'session'));
    if (gen !== 'completed') DecimalU64(gen);
    return ['slot', 'generic_questions'];
  }
  check(gen === undefined);
  switch (kind) {
    case 'question': return ['publication', 'generic_questions'];
    case 'question-fallback': return ['slot', 'generic_questions'];
    case 'native': return ['publication', 'native_approval'];
    case 'legacy': return ['legacy', 'legacy_approval'];
    default: throw new InvalidWire();
  }
}
