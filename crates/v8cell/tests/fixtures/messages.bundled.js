var yt = Object.defineProperty;
var n = (e, t) => yt(e, "name", { value: t, configurable: !0 });

// node_modules/convex/dist/esm/values/base64.js
var A = [], b = [], wt = Uint8Array, ce = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
for (I = 0, Oe = ce.length; I < Oe; ++I)
  A[I] = ce[I], b[ce.charCodeAt(I)] = I;
var I, Oe;
b[45] = 62;
b[95] = 63;
function gt(e) {
  var t = e.length;
  if (t % 4 > 0)
    throw new Error("Invalid string. Length must be a multiple of 4");
  var r = e.indexOf("=");
  r === -1 && (r = t);
  var o = r === t ? 0 : 4 - r % 4;
  return [r, o];
}
n(gt, "getLens");
function xt(e, t, r) {
  return (t + r) * 3 / 4 - r;
}
n(xt, "_byteLength");
function P(e) {
  var t, r = gt(e), o = r[0], s = r[1], a = new wt(xt(e, o, s)), i = 0, f = s > 0 ? o - 4 : o, p;
  for (p = 0; p < f; p += 4)
    t = b[e.charCodeAt(p)] << 18 | b[e.charCodeAt(p + 1)] << 12 | b[e.charCodeAt(p + 2)] << 6 | b[e.charCodeAt(p + 3)], a[i++] = t >> 16 & 255, a[i++] = t >> 8 & 255, a[i++] = t & 255;
  return s === 2 && (t = b[e.charCodeAt(p)] << 2 | b[e.charCodeAt(p + 1)] >> 4, a[i++] = t & 255), s === 1 && (t = b[e.charCodeAt(p)] << 10 | b[e.charCodeAt(p + 1)] << 4 | b[e.charCodeAt(p + 2)] >> 2, a[i++] = t >> 8 & 255, a[i++] = t & 255), a;
}
n(P, "toByteArray");
function bt(e) {
  return A[e >> 18 & 63] + A[e >> 12 & 63] + A[e >> 6 & 63] + A[e & 63];
}
n(bt, "tripletToBase64");
function vt(e, t, r) {
  for (var o, s = [], a = t; a < r; a += 3)
    o = (e[a] << 16 & 16711680) + (e[a + 1] << 8 & 65280) + (e[a + 2] & 255), s.push(bt(o));
  return s.join("");
}
n(vt, "encodeChunk");
function F(e) {
  for (var t, r = e.length, o = r % 3, s = [], a = 16383, i = 0, f = r - o; i < f; i += a)
    s.push(
      vt(
        e,
        i,
        i + a > f ? f : i + a
      )
    );
  return o === 1 ? (t = e[r - 1], s.push(A[t >> 2] + A[t << 4 & 63] + "==")) : o === 2 && (t = (e[r - 2] << 8) + e[r - 1], s.push(
    A[t >> 10] + A[t >> 4 & 63] + A[t << 2 & 63] + "="
  )), s.join("");
}
n(F, "fromByteArray");

// node_modules/convex/dist/esm/common/index.js
function T(e) {
  if (e === void 0)
    return {};
  if (!V(e))
    throw new Error(
      `The arguments to a Convex function must be an object. Received: ${e}`
    );
  return e;
}
n(T, "parseArgs");
function V(e) {
  let t = typeof e == "object", r = Object.getPrototypeOf(e), o = r === null || r === Object.prototype || // Objects generated from other contexts (e.g. across Node.js `vm` modules) will not satisfy the previous
  // conditions but are still simple objects.
  r?.constructor?.name === "Object";
  return t && o;
}
n(V, "isSimpleObject");

// node_modules/convex/dist/esm/values/value.js
var Pe = !0, C = BigInt("-9223372036854775808"), pe = BigInt("9223372036854775807"), le = BigInt("0"), At = BigInt("8"), Et = BigInt("256");
function Fe(e) {
  return Number.isNaN(e) || !Number.isFinite(e) || Object.is(e, -0);
}
n(Fe, "isSpecial");
function St(e) {
  e < le && (e -= C + C);
  let t = e.toString(16);
  t.length % 2 === 1 && (t = "0" + t);
  let r = new Uint8Array(new ArrayBuffer(8)), o = 0;
  for (let s of t.match(/.{2}/g).reverse())
    r.set([parseInt(s, 16)], o++), e >>= At;
  return F(r);
}
n(St, "slowBigIntToBase64");
function It(e) {
  let t = P(e);
  if (t.byteLength !== 8)
    throw new Error(
      `Received ${t.byteLength} bytes, expected 8 for $integer`
    );
  let r = le, o = le;
  for (let s of t)
    r += BigInt(s) * Et ** o, o++;
  return r > pe && (r += C + C), r;
}
n(It, "slowBase64ToBigInt");
function Tt(e) {
  if (e < C || pe < e)
    throw new Error(
      `BigInt ${e} does not fit into a 64-bit signed integer.`
    );
  let t = new ArrayBuffer(8);
  return new DataView(t).setBigInt64(0, e, !0), F(new Uint8Array(t));
}
n(Tt, "modernBigIntToBase64");
function _t(e) {
  let t = P(e);
  if (t.byteLength !== 8)
    throw new Error(
      `Received ${t.byteLength} bytes, expected 8 for $integer`
    );
  return new DataView(t.buffer).getBigInt64(0, !0);
}
n(_t, "modernBase64ToBigInt");
var Ot = DataView.prototype.setBigInt64 ? Tt : St, Ct = DataView.prototype.getBigInt64 ? _t : It, $e = 1024;
function fe(e) {
  if (e.length > $e)
    throw new Error(
      `Field name ${e} exceeds maximum field name length ${$e}.`
    );
  if (e.startsWith("$"))
    throw new Error(`Field name ${e} starts with a '$', which is reserved.`);
  for (let t = 0; t < e.length; t += 1) {
    let r = e.charCodeAt(t);
    if (r < 32 || r >= 127)
      throw new Error(
        `Field name ${e} has invalid character '${e[t]}': Field names can only contain non-control ASCII characters`
      );
  }
}
n(fe, "validateObjectField");
function y(e) {
  if (e === null || typeof e == "boolean" || typeof e == "number" || typeof e == "string")
    return e;
  if (Array.isArray(e))
    return e.map((o) => y(o));
  if (typeof e != "object")
    throw new Error(`Unexpected type of ${e}`);
  let t = Object.entries(e);
  if (t.length === 1) {
    let o = t[0][0];
    if (o === "$bytes") {
      if (typeof e.$bytes != "string")
        throw new Error(`Malformed $bytes field on ${e}`);
      return P(e.$bytes).buffer;
    }
    if (o === "$integer") {
      if (typeof e.$integer != "string")
        throw new Error(`Malformed $integer field on ${e}`);
      return Ct(e.$integer);
    }
    if (o === "$float") {
      if (typeof e.$float != "string")
        throw new Error(`Malformed $float field on ${e}`);
      let s = P(e.$float);
      if (s.byteLength !== 8)
        throw new Error(
          `Received ${s.byteLength} bytes, expected 8 for $float`
        );
      let i = new DataView(s.buffer).getFloat64(0, Pe);
      if (!Fe(i))
        throw new Error(`Float ${i} should be encoded as a number`);
      return i;
    }
    if (o === "$set")
      throw new Error(
        "Received a Set which is no longer supported as a Convex type."
      );
    if (o === "$map")
      throw new Error(
        "Received a Map which is no longer supported as a Convex type."
      );
  }
  let r = {};
  for (let [o, s] of Object.entries(e))
    fe(o), r[o] = y(s);
  return r;
}
n(y, "jsonToConvex");
var Ne = 16384;
function _(e) {
  let t = JSON.stringify(e, (r, o) => o === void 0 ? "undefined" : typeof o == "bigint" ? `${o.toString()}n` : o);
  if (t.length > Ne) {
    let r = "[...truncated]", o = Ne - r.length, s = t.codePointAt(o - 1);
    return s !== void 0 && s > 65535 && (o -= 1), t.substring(0, o) + r;
  }
  return t;
}
n(_, "stringifyValueForError");
function R(e, t, r, o) {
  if (e === void 0) {
    let i = r && ` (present at path ${r} in original object ${_(
      t
    )})`;
    throw new Error(
      `undefined is not a valid Convex value${i}. To learn about Convex's supported types, see https://docs.convex.dev/using/types.`
    );
  }
  if (e === null)
    return e;
  if (typeof e == "bigint") {
    if (e < C || pe < e)
      throw new Error(
        `BigInt ${e} does not fit into a 64-bit signed integer.`
      );
    return { $integer: Ot(e) };
  }
  if (typeof e == "number")
    if (Fe(e)) {
      let i = new ArrayBuffer(8);
      return new DataView(i).setFloat64(0, e, Pe), { $float: F(new Uint8Array(i)) };
    } else
      return e;
  if (typeof e == "boolean" || typeof e == "string")
    return e;
  if (e instanceof ArrayBuffer)
    return { $bytes: F(new Uint8Array(e)) };
  if (Array.isArray(e))
    return e.map(
      (i, f) => R(i, t, r + `[${f}]`, !1)
    );
  if (e instanceof Set)
    throw new Error(
      ue(r, "Set", [...e], t)
    );
  if (e instanceof Map)
    throw new Error(
      ue(r, "Map", [...e], t)
    );
  if (!V(e)) {
    let i = e?.constructor?.name, f = i ? `${i} ` : "";
    throw new Error(
      ue(r, f, e, t)
    );
  }
  let s = {}, a = Object.entries(e);
  a.sort(([i, f], [p, N]) => i === p ? 0 : i < p ? -1 : 1);
  for (let [i, f] of a)
    f !== void 0 ? (fe(i), s[i] = R(f, t, r + `.${i}`, !1)) : o && (fe(i), s[i] = Re(
      f,
      t,
      r + `.${i}`
    ));
  return s;
}
n(R, "convexToJsonInternal");
function ue(e, t, r, o) {
  return e ? `${t}${_(
    r
  )} is not a supported Convex type (present at path ${e} in original object ${_(
    o
  )}). To learn about Convex's supported types, see https://docs.convex.dev/using/types.` : `${t}${_(
    r
  )} is not a supported Convex type.`;
}
n(ue, "errorMessageForUnsupportedType");
function Re(e, t, r) {
  if (e === void 0)
    return { $undefined: null };
  if (t === void 0)
    throw new Error(
      `Programming error. Current value is ${_(
        e
      )} but original value is undefined`
    );
  return R(e, t, r, !1);
}
n(Re, "convexOrUndefinedToJsonInternal");
function m(e) {
  return R(e, e, "", !1);
}
n(m, "convexToJson");
function v(e) {
  return Re(e, e, "");
}
n(v, "convexOrUndefinedToJson");
function qe(e) {
  return R(e, e, "", !0);
}
n(qe, "patchValueToJson");

// node_modules/convex/dist/esm/values/validators.js
var $t = Object.defineProperty, Nt = /* @__PURE__ */ n((e, t, r) => t in e ? $t(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), h = /* @__PURE__ */ n((e, t, r) => Nt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), Pt = "https://docs.convex.dev/error#undefined-validator";
function q(e, t) {
  let r = t !== void 0 ? ` for field "${t}"` : "";
  throw new Error(
    `A validator is undefined${r} in ${e}. This is often caused by circular imports. See ${Pt} for details.`
  );
}
n(q, "throwUndefinedValidatorError");
var x = class {
  static {
    n(this, "BaseValidator");
  }
  constructor({ isOptional: t }) {
    h(this, "type"), h(this, "fieldPaths"), h(this, "isOptional"), h(this, "isConvexValidator"), this.isOptional = t, this.isConvexValidator = !0;
  }
}, L = class e extends x {
  static {
    n(this, "VId");
  }
  /**
   * Usually you'd use `v.id(tableName)` instead.
   */
  constructor({
    isOptional: t,
    tableName: r
  }) {
    if (super({ isOptional: t }), h(this, "tableName"), h(this, "kind", "id"), typeof r != "string")
      throw new Error("v.id(tableName) requires a string");
    this.tableName = r;
  }
  /** @internal */
  get json() {
    return { type: "id", tableName: this.tableName };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      tableName: this.tableName
    });
  }
}, j = class e extends x {
  static {
    n(this, "VFloat64");
  }
  constructor() {
    super(...arguments), h(this, "kind", "float64");
  }
  /** @internal */
  get json() {
    return { type: "number" };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional"
    });
  }
}, M = class e extends x {
  static {
    n(this, "VInt64");
  }
  constructor() {
    super(...arguments), h(this, "kind", "int64");
  }
  /** @internal */
  get json() {
    return { type: "bigint" };
  }
  /** @internal */
  asOptional() {
    return new e({ isOptional: "optional" });
  }
}, k = class e extends x {
  static {
    n(this, "VBoolean");
  }
  constructor() {
    super(...arguments), h(this, "kind", "boolean");
  }
  /** @internal */
  get json() {
    return { type: this.kind };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional"
    });
  }
}, D = class e extends x {
  static {
    n(this, "VBytes");
  }
  constructor() {
    super(...arguments), h(this, "kind", "bytes");
  }
  /** @internal */
  get json() {
    return { type: this.kind };
  }
  /** @internal */
  asOptional() {
    return new e({ isOptional: "optional" });
  }
}, Q = class e extends x {
  static {
    n(this, "VString");
  }
  constructor() {
    super(...arguments), h(this, "kind", "string");
  }
  /** @internal */
  get json() {
    return { type: this.kind };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional"
    });
  }
}, G = class e extends x {
  static {
    n(this, "VNull");
  }
  constructor() {
    super(...arguments), h(this, "kind", "null");
  }
  /** @internal */
  get json() {
    return { type: this.kind };
  }
  /** @internal */
  asOptional() {
    return new e({ isOptional: "optional" });
  }
}, H = class e extends x {
  static {
    n(this, "VAny");
  }
  constructor() {
    super(...arguments), h(this, "kind", "any");
  }
  /** @internal */
  get json() {
    return {
      type: this.kind
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional"
    });
  }
}, z = class e extends x {
  static {
    n(this, "VObject");
  }
  /**
   * Usually you'd use `v.object({ ... })` instead.
   */
  constructor({
    isOptional: t,
    fields: r
  }) {
    super({ isOptional: t }), h(this, "fields"), h(this, "kind", "object"), globalThis.Object.entries(r).forEach(([o, s]) => {
      if (s === void 0 && q("v.object()", o), !s.isConvexValidator)
        throw new Error("v.object() entries must be validators");
    }), this.fields = r;
  }
  /** @internal */
  get json() {
    return {
      type: this.kind,
      value: globalThis.Object.fromEntries(
        globalThis.Object.entries(this.fields).map(([t, r]) => [
          t,
          {
            fieldType: r.json,
            optional: r.isOptional === "optional"
          }
        ])
      )
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      fields: this.fields
    });
  }
  /**
   * Create a new VObject with the specified fields omitted.
   * @param fields The field names to omit from this VObject.
   */
  omit(...t) {
    let r = { ...this.fields };
    for (let o of t)
      delete r[o];
    return new e({
      isOptional: this.isOptional,
      fields: r
    });
  }
  /**
   * Create a new VObject with only the specified fields.
   * @param fields The field names to pick from this VObject.
   */
  pick(...t) {
    let r = {};
    for (let o of t)
      r[o] = this.fields[o];
    return new e({
      isOptional: this.isOptional,
      fields: r
    });
  }
  /**
   * Create a new VObject with all fields marked as optional.
   */
  partial() {
    let t = {};
    for (let [r, o] of globalThis.Object.entries(this.fields))
      t[r] = o.asOptional();
    return new e({
      isOptional: this.isOptional,
      fields: t
    });
  }
  /**
   * Create a new VObject with additional fields merged in.
   * @param fields An object with additional validators to merge into this VObject.
   */
  extend(t) {
    return new e({
      isOptional: this.isOptional,
      fields: { ...this.fields, ...t }
    });
  }
}, W = class e extends x {
  static {
    n(this, "VLiteral");
  }
  /**
   * Usually you'd use `v.literal(value)` instead.
   */
  constructor({ isOptional: t, value: r }) {
    if (super({ isOptional: t }), h(this, "value"), h(this, "kind", "literal"), typeof r != "string" && typeof r != "boolean" && typeof r != "number" && typeof r != "bigint")
      throw new Error("v.literal(value) must be a string, number, or boolean");
    this.value = r;
  }
  /** @internal */
  get json() {
    return {
      type: this.kind,
      value: m(this.value)
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      value: this.value
    });
  }
}, Y = class e extends x {
  static {
    n(this, "VArray");
  }
  /**
   * Usually you'd use `v.array(element)` instead.
   */
  constructor({
    isOptional: t,
    element: r
  }) {
    super({ isOptional: t }), h(this, "element"), h(this, "kind", "array"), r === void 0 && q("v.array()"), this.element = r;
  }
  /** @internal */
  get json() {
    return {
      type: this.kind,
      value: this.element.json
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      element: this.element
    });
  }
}, X = class e extends x {
  static {
    n(this, "VRecord");
  }
  /**
   * Usually you'd use `v.record(key, value)` instead.
   */
  constructor({
    isOptional: t,
    key: r,
    value: o
  }) {
    if (super({ isOptional: t }), h(this, "key"), h(this, "value"), h(this, "kind", "record"), r === void 0 && q("v.record()", "key"), o === void 0 && q("v.record()", "value"), r.isOptional === "optional")
      throw new Error("Record validator cannot have optional keys");
    if (o.isOptional === "optional")
      throw new Error("Record validator cannot have optional values");
    if (!r.isConvexValidator || !o.isConvexValidator)
      throw new Error("Key and value of v.record() but be validators");
    this.key = r, this.value = o;
  }
  /** @internal */
  get json() {
    return {
      type: this.kind,
      // This cast is needed because TypeScript thinks the key type is too wide
      keys: this.key.json,
      values: {
        fieldType: this.value.json,
        optional: !1
      }
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      key: this.key,
      value: this.value
    });
  }
}, K = class e extends x {
  static {
    n(this, "VUnion");
  }
  /**
   * Usually you'd use `v.union(...members)` instead.
   */
  constructor({ isOptional: t, members: r }) {
    super({ isOptional: t }), h(this, "members"), h(this, "kind", "union"), r.forEach((o, s) => {
      if (o === void 0 && q("v.union()", `member at index ${s}`), !o.isConvexValidator)
        throw new Error("All members of v.union() must be validators");
    }), this.members = r;
  }
  /** @internal */
  get json() {
    return {
      type: this.kind,
      value: this.members.map((t) => t.json)
    };
  }
  /** @internal */
  asOptional() {
    return new e({
      isOptional: "optional",
      members: this.members
    });
  }
};

// node_modules/convex/dist/esm/values/validator.js
function de(e) {
  return !!e.isConvexValidator;
}
n(de, "isValidator");
function Z(e) {
  return de(e) ? e : c.object(e);
}
n(Z, "asObjectValidator");
var c = {
  /**
   * Validates that the value is a document ID for the given table.
   *
   * IDs are strings at runtime but are typed as `Id<"tableName">` in
   * TypeScript for type safety.
   *
   * @example
   * ```typescript
   * args: { userId: v.id("users") }
   * ```
   *
   * @param tableName The name of the table.
   */
  id: /* @__PURE__ */ n((e) => new L({
    isOptional: "required",
    tableName: e
  }), "id"),
  /**
   * Validates that the value is `null`.
   *
   * Use `returns: v.null()` for functions that don't return a meaningful value.
   * JavaScript `undefined` is not a valid Convex value, it is automatically
   * converted to `null`.
   */
  null: /* @__PURE__ */ n(() => new G({ isOptional: "required" }), "null"),
  /**
   * Validates that the value is a JavaScript `number` (Convex Float64).
   *
   * Supports all IEEE-754 double-precision floating point numbers including
   * NaN and Infinity.
   *
   * Alias for `v.float64()`.
   */
  number: /* @__PURE__ */ n(() => new j({ isOptional: "required" }), "number"),
  /**
   * Validates that the value is a JavaScript `number` (Convex Float64).
   *
   * Supports all IEEE-754 double-precision floating point numbers.
   */
  float64: /* @__PURE__ */ n(() => new j({ isOptional: "required" }), "float64"),
  /**
   * @deprecated Use `v.int64()` instead.
   */
  bigint: /* @__PURE__ */ n(() => new M({ isOptional: "required" }), "bigint"),
  /**
   * Validates that the value is a JavaScript `bigint` (Convex Int64).
   *
   * Supports BigInts between -2^63 and 2^63-1.
   *
   * @example
   * ```typescript
   * args: { timestamp: v.int64() }
   * // Usage: createDoc({ timestamp: 1234567890n })
   * ```
   */
  int64: /* @__PURE__ */ n(() => new M({ isOptional: "required" }), "int64"),
  /**
   * Validates that the value is a `boolean`.
   */
  boolean: /* @__PURE__ */ n(() => new k({ isOptional: "required" }), "boolean"),
  /**
   * Validates that the value is a `string`.
   *
   * Strings are stored as UTF-8 and their storage size is calculated as their
   * UTF-8 encoded size.
   */
  string: /* @__PURE__ */ n(() => new Q({ isOptional: "required" }), "string"),
  /**
   * Validates that the value is an `ArrayBuffer` (Convex Bytes).
   *
   * Use for binary data.
   */
  bytes: /* @__PURE__ */ n(() => new D({ isOptional: "required" }), "bytes"),
  /**
   * Validates that the value is exactly equal to the given literal.
   *
   * Useful for discriminated unions and enum-like patterns.
   *
   * @example
   * ```typescript
   * // Discriminated union pattern:
   * v.union(
   *   v.object({ kind: v.literal("error"), message: v.string() }),
   *   v.object({ kind: v.literal("success"), value: v.number() }),
   * )
   * ```
   *
   * @param literal The literal value to compare against.
   */
  literal: /* @__PURE__ */ n((e) => new W({ isOptional: "required", value: e }), "literal"),
  /**
   * Validates that the value is an `Array` where every element matches the
   * given validator.
   *
   * Arrays can have at most 8192 elements.
   *
   * @example
   * ```typescript
   * args: { tags: v.array(v.string()) }
   * args: { coordinates: v.array(v.number()) }
   * args: { items: v.array(v.object({ name: v.string(), qty: v.number() })) }
   * ```
   *
   * @param element The validator for the elements of the array.
   */
  array: /* @__PURE__ */ n((e) => new Y({ isOptional: "required", element: e }), "array"),
  /**
   * Validates that the value is an `Object` with the specified properties.
   *
   * Objects can have at most 1024 entries. Field names must be non-empty and
   * must not start with `"$"` or `"_"` (`_` is reserved for system fields
   * like `_id` and `_creationTime`; `$` is reserved for Convex internal use).
   *
   * @example
   * ```typescript
   * args: {
   *   user: v.object({
   *     name: v.string(),
   *     email: v.string(),
   *     age: v.optional(v.number()),
   *   })
   * }
   * ```
   *
   * @param fields An object mapping property names to their validators.
   */
  object: /* @__PURE__ */ n((e) => new z({ isOptional: "required", fields: e }), "object"),
  /**
   * Validates that the value is a `Record` (object with dynamic keys).
   *
   * Records are objects at runtime but allow dynamic keys, unlike `v.object()`
   * which requires known property names. Keys must be ASCII characters only,
   * non-empty, and not start with `"$"` or `"_"`.
   *
   * @example
   * ```typescript
   * // Map of user IDs to scores:
   * args: { scores: v.record(v.id("users"), v.number()) }
   *
   * // Map of string keys to string values:
   * args: { metadata: v.record(v.string(), v.string()) }
   * ```
   *
   * @param keys The validator for the keys of the record.
   * @param values The validator for the values of the record.
   */
  record: /* @__PURE__ */ n((e, t) => new X({
    isOptional: "required",
    key: e,
    value: t
  }), "record"),
  /**
   * Validates that the value matches at least one of the given validators.
   *
   * @example
   * ```typescript
   * // Allow string or number:
   * args: { value: v.union(v.string(), v.number()) }
   *
   * // Discriminated union (recommended pattern):
   * v.union(
   *   v.object({ kind: v.literal("text"), body: v.string() }),
   *   v.object({ kind: v.literal("image"), url: v.string() }),
   * )
   *
   * // Nullable value:
   * returns: v.union(v.object({ ... }), v.null())
   * ```
   *
   * @param members The validators to match against.
   */
  union: /* @__PURE__ */ n((...e) => new K({
    isOptional: "required",
    members: e
  }), "union"),
  /**
   * A validator that accepts any Convex value without validation.
   *
   * Prefer using specific validators when possible for better type safety
   * and runtime validation.
   */
  any: /* @__PURE__ */ n(() => new H({ isOptional: "required" }), "any"),
  /**
   * Makes a property optional in an object validator.
   *
   * An optional property can be omitted entirely when creating a document or
   * calling a function. This is different from `v.nullable()` which requires
   * the property to be present but allows `null`.
   *
   * @example
   * ```typescript
   * v.object({
   *   name: v.string(),              // required
   *   nickname: v.optional(v.string()), // can be omitted
   * })
   *
   * // Valid: { name: "Alice" }
   * // Valid: { name: "Alice", nickname: "Ali" }
   * // Invalid: { name: "Alice", nickname: null }  - use v.nullable() for this
   * ```
   *
   * @param value The property value validator to make optional.
   */
  optional: /* @__PURE__ */ n((e) => e.asOptional(), "optional"),
  /**
   * Allows a value to be either the given type or `null`.
   *
   * This is shorthand for `v.union(value, v.null())`. Unlike `v.optional()`,
   * the property must still be present, but may be `null`.
   *
   * @example
   * ```typescript
   * v.object({
   *   name: v.string(),
   *   deletedAt: v.nullable(v.number()), // must be present, can be null
   * })
   *
   * // Valid: { name: "Alice", deletedAt: null }
   * // Valid: { name: "Alice", deletedAt: 1234567890 }
   * // Invalid: { name: "Alice" }  - deletedAt is required
   * ```
   */
  nullable: /* @__PURE__ */ n((e) => c.union(e, c.null()), "nullable")
};

// node_modules/convex/dist/esm/values/errors.js
var Ft = Object.defineProperty, Rt = /* @__PURE__ */ n((e, t, r) => t in e ? Ft(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), he = /* @__PURE__ */ n((e, t, r) => Rt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), je, Me, qt = Symbol.for("ConvexError"), ee = class extends (Me = Error, je = qt, Me) {
  static {
    n(this, "ConvexError");
  }
  constructor(t) {
    super(typeof t == "string" ? t : _(t)), he(this, "name", "ConvexError"), he(this, "data"), he(this, je, !0), this.data = t;
  }
};

// node_modules/convex/dist/esm/values/compare_utf8.js
var Be = /* @__PURE__ */ n(() => Array.from({ length: 4 }, () => 0), "arr"), Br = Be(), Ur = Be();

// node_modules/convex/dist/esm/index.js
var g = "1.37.0";

// node_modules/convex/dist/esm/server/impl/syscall.js
function B(e, t) {
  if (typeof Convex > "u" || Convex.syscall === void 0)
    throw new Error(
      "The Convex database and auth objects are being used outside of a Convex backend. Did you mean to use `useQuery` or `useMutation` to call a Convex function?"
    );
  let r = Convex.syscall(e, JSON.stringify(t));
  return JSON.parse(r);
}
n(B, "performSyscall");
async function u(e, t) {
  if (typeof Convex > "u" || Convex.asyncSyscall === void 0)
    throw new Error(
      "The Convex database and auth objects are being used outside of a Convex backend. Did you mean to use `useQuery` or `useMutation` to call a Convex function?"
    );
  let r;
  try {
    r = await Convex.asyncSyscall(e, JSON.stringify(t));
  } catch (o) {
    if (o.data !== void 0) {
      let s = new ee(o.message);
      throw s.data = y(o.data), s;
    }
    throw new Error(o.message);
  }
  return JSON.parse(r);
}
n(u, "performAsyncSyscall");

// node_modules/convex/dist/esm/server/functionName.js
var U = Symbol.for("functionName");

// node_modules/convex/dist/esm/server/components/paths.js
var Ue = Symbol.for("toReferencePath");
function jt(e) {
  return e[Ue] ?? null;
}
n(jt, "extractReferencePath");
function Mt(e) {
  return e.startsWith("function://");
}
n(Mt, "isFunctionHandle");
function E(e) {
  let t;
  if (typeof e == "string")
    Mt(e) ? t = { functionHandle: e } : t = { name: e };
  else if (e[U])
    t = { name: e[U] };
  else {
    let r = jt(e);
    if (!r)
      throw new Error(`${e} is not a functionReference`);
    t = { reference: r };
  }
  return t;
}
n(E, "getFunctionAddress");

// node_modules/convex/dist/esm/server/impl/validate.js
function l(e, t, r, o) {
  if (e === void 0)
    throw new TypeError(
      `Must provide arg ${t} \`${o}\` to \`${r}\``
    );
}
n(l, "validateArg");
function Je(e, t, r, o) {
  if (!Number.isInteger(e) || e < 0)
    throw new TypeError(
      `Arg ${t} \`${o}\` to \`${r}\` must be a non-negative integer`
    );
}
n(Je, "validateArgIsNonNegativeInteger");

// node_modules/convex/dist/esm/server/impl/authentication_impl.js
function me(e) {
  return {
    getUserIdentity: /* @__PURE__ */ n(async () => await u("1.0/getUserIdentity", {
      requestId: e
    }), "getUserIdentity")
  };
}
n(me, "setupAuth");

// node_modules/convex/dist/esm/server/filter_builder.js
var Bt = Object.defineProperty, Ut = /* @__PURE__ */ n((e, t, r) => t in e ? Bt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), Ve = /* @__PURE__ */ n((e, t, r) => Ut(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), te = class {
  static {
    n(this, "Expression");
  }
  /**
   * @internal
   */
  constructor() {
    Ve(this, "_isExpression"), Ve(this, "_value");
  }
};

// node_modules/convex/dist/esm/server/impl/filter_builder_impl.js
var Jt = Object.defineProperty, Vt = /* @__PURE__ */ n((e, t, r) => t in e ? Jt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), Lt = /* @__PURE__ */ n((e, t, r) => Vt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), w = class extends te {
  static {
    n(this, "ExpressionImpl");
  }
  constructor(t) {
    super(), Lt(this, "inner"), this.inner = t;
  }
  serialize() {
    return this.inner;
  }
};
function d(e) {
  return e instanceof w ? e.serialize() : { $literal: v(e) };
}
n(d, "serializeExpression");
var Le = {
  //  Comparisons  /////////////////////////////////////////////////////////////
  eq(e, t) {
    return new w({
      $eq: [d(e), d(t)]
    });
  },
  neq(e, t) {
    return new w({
      $neq: [d(e), d(t)]
    });
  },
  lt(e, t) {
    return new w({
      $lt: [d(e), d(t)]
    });
  },
  lte(e, t) {
    return new w({
      $lte: [d(e), d(t)]
    });
  },
  gt(e, t) {
    return new w({
      $gt: [d(e), d(t)]
    });
  },
  gte(e, t) {
    return new w({
      $gte: [d(e), d(t)]
    });
  },
  //  Arithmetic  //////////////////////////////////////////////////////////////
  add(e, t) {
    return new w({
      $add: [d(e), d(t)]
    });
  },
  sub(e, t) {
    return new w({
      $sub: [d(e), d(t)]
    });
  },
  mul(e, t) {
    return new w({
      $mul: [d(e), d(t)]
    });
  },
  div(e, t) {
    return new w({
      $div: [d(e), d(t)]
    });
  },
  mod(e, t) {
    return new w({
      $mod: [d(e), d(t)]
    });
  },
  neg(e) {
    return new w({ $neg: d(e) });
  },
  //  Logic  ///////////////////////////////////////////////////////////////////
  and(...e) {
    return new w({ $and: e.map(d) });
  },
  or(...e) {
    return new w({ $or: e.map(d) });
  },
  not(e) {
    return new w({ $not: d(e) });
  },
  //  Other  ///////////////////////////////////////////////////////////////////
  field(e) {
    return new w({ $field: e });
  }
};

// node_modules/convex/dist/esm/server/index_range_builder.js
var kt = Object.defineProperty, Dt = /* @__PURE__ */ n((e, t, r) => t in e ? kt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), Qt = /* @__PURE__ */ n((e, t, r) => Dt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), re = class {
  static {
    n(this, "IndexRange");
  }
  /**
   * @internal
   */
  constructor() {
    Qt(this, "_isIndexRange");
  }
};

// node_modules/convex/dist/esm/server/impl/index_range_builder_impl.js
var Gt = Object.defineProperty, Ht = /* @__PURE__ */ n((e, t, r) => t in e ? Gt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), ke = /* @__PURE__ */ n((e, t, r) => Ht(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), ne = class e extends re {
  static {
    n(this, "IndexRangeBuilderImpl");
  }
  constructor(t) {
    super(), ke(this, "rangeExpressions"), ke(this, "isConsumed"), this.rangeExpressions = t, this.isConsumed = !1;
  }
  static new() {
    return new e([]);
  }
  consume() {
    if (this.isConsumed)
      throw new Error(
        "IndexRangeBuilder has already been used! Chain your method calls like `q => q.eq(...).eq(...)`. See https://docs.convex.dev/using/indexes"
      );
    this.isConsumed = !0;
  }
  eq(t, r) {
    return this.consume(), new e(
      this.rangeExpressions.concat({
        type: "Eq",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  gt(t, r) {
    return this.consume(), new e(
      this.rangeExpressions.concat({
        type: "Gt",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  gte(t, r) {
    return this.consume(), new e(
      this.rangeExpressions.concat({
        type: "Gte",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  lt(t, r) {
    return this.consume(), new e(
      this.rangeExpressions.concat({
        type: "Lt",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  lte(t, r) {
    return this.consume(), new e(
      this.rangeExpressions.concat({
        type: "Lte",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  export() {
    return this.consume(), this.rangeExpressions;
  }
};

// node_modules/convex/dist/esm/server/search_filter_builder.js
var zt = Object.defineProperty, Wt = /* @__PURE__ */ n((e, t, r) => t in e ? zt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), Yt = /* @__PURE__ */ n((e, t, r) => Wt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), oe = class {
  static {
    n(this, "SearchFilter");
  }
  /**
   * @internal
   */
  constructor() {
    Yt(this, "_isSearchFilter");
  }
};

// node_modules/convex/dist/esm/server/impl/search_filter_builder_impl.js
var Xt = Object.defineProperty, Kt = /* @__PURE__ */ n((e, t, r) => t in e ? Xt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), De = /* @__PURE__ */ n((e, t, r) => Kt(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), se = class e extends oe {
  static {
    n(this, "SearchFilterBuilderImpl");
  }
  constructor(t) {
    super(), De(this, "filters"), De(this, "isConsumed"), this.filters = t, this.isConsumed = !1;
  }
  static new() {
    return new e([]);
  }
  consume() {
    if (this.isConsumed)
      throw new Error(
        "SearchFilterBuilder has already been used! Chain your method calls like `q => q.search(...).eq(...)`."
      );
    this.isConsumed = !0;
  }
  search(t, r) {
    return l(t, 1, "search", "fieldName"), l(r, 2, "search", "query"), this.consume(), new e(
      this.filters.concat({
        type: "Search",
        fieldPath: t,
        value: r
      })
    );
  }
  eq(t, r) {
    return l(t, 1, "eq", "fieldName"), arguments.length !== 2 && l(r, 2, "search", "value"), this.consume(), new e(
      this.filters.concat({
        type: "Eq",
        fieldPath: t,
        value: v(r)
      })
    );
  }
  export() {
    return this.consume(), this.filters;
  }
};

// node_modules/convex/dist/esm/server/impl/query_impl.js
var Zt = Object.defineProperty, er = /* @__PURE__ */ n((e, t, r) => t in e ? Zt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), ye = /* @__PURE__ */ n((e, t, r) => er(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), Qe = 256, $ = class {
  static {
    n(this, "QueryInitializerImpl");
  }
  constructor(t) {
    ye(this, "tableName"), this.tableName = t;
  }
  withIndex(t, r) {
    l(t, 1, "withIndex", "indexName");
    let o = ne.new();
    return r !== void 0 && (o = r(o)), new O({
      source: {
        type: "IndexRange",
        indexName: this.tableName + "." + t,
        range: o.export(),
        order: null
      },
      operators: []
    });
  }
  withSearchIndex(t, r) {
    l(t, 1, "withSearchIndex", "indexName"), l(r, 2, "withSearchIndex", "searchFilter");
    let o = se.new();
    return new O({
      source: {
        type: "Search",
        indexName: this.tableName + "." + t,
        filters: r(o).export()
      },
      operators: []
    });
  }
  fullTableScan() {
    return new O({
      source: {
        type: "FullTableScan",
        tableName: this.tableName,
        order: null
      },
      operators: []
    });
  }
  order(t) {
    return this.fullTableScan().order(t);
  }
  // This is internal API and should not be exposed to developers yet.
  async count() {
    let t = await u("1.0/count", {
      table: this.tableName
    });
    return y(t);
  }
  filter(t) {
    return this.fullTableScan().filter(t);
  }
  limit(t) {
    return this.fullTableScan().limit(t);
  }
  collect() {
    return this.fullTableScan().collect();
  }
  take(t) {
    return this.fullTableScan().take(t);
  }
  paginate(t) {
    return this.fullTableScan().paginate(t);
  }
  first() {
    return this.fullTableScan().first();
  }
  unique() {
    return this.fullTableScan().unique();
  }
  [Symbol.asyncIterator]() {
    return this.fullTableScan()[Symbol.asyncIterator]();
  }
};
function Ge(e) {
  throw new Error(
    e === "consumed" ? "This query is closed and can't emit any more values." : "This query has been chained with another operator and can't be reused."
  );
}
n(Ge, "throwClosedError");
var O = class e {
  static {
    n(this, "QueryImpl");
  }
  constructor(t) {
    ye(this, "state"), ye(this, "tableNameForErrorMessages"), this.state = { type: "preparing", query: t }, t.source.type === "FullTableScan" ? this.tableNameForErrorMessages = t.source.tableName : this.tableNameForErrorMessages = t.source.indexName.split(".")[0];
  }
  takeQuery() {
    if (this.state.type !== "preparing")
      throw new Error(
        "A query can only be chained once and can't be chained after iteration begins."
      );
    let t = this.state.query;
    return this.state = { type: "closed" }, t;
  }
  startQuery() {
    if (this.state.type === "executing")
      throw new Error("Iteration can only begin on a query once.");
    (this.state.type === "closed" || this.state.type === "consumed") && Ge(this.state.type);
    let t = this.state.query, { queryId: r } = B("1.0/queryStream", { query: t, version: g });
    return this.state = { type: "executing", queryId: r }, r;
  }
  closeQuery() {
    if (this.state.type === "executing") {
      let t = this.state.queryId;
      B("1.0/queryCleanup", { queryId: t });
    }
    this.state = { type: "consumed" };
  }
  order(t) {
    l(t, 1, "order", "order");
    let r = this.takeQuery();
    if (r.source.type === "Search")
      throw new Error(
        "Search queries must always be in relevance order. Can not set order manually."
      );
    if (r.source.order !== null)
      throw new Error("Queries may only specify order at most once");
    return r.source.order = t, new e(r);
  }
  filter(t) {
    l(t, 1, "filter", "predicate");
    let r = this.takeQuery();
    if (r.operators.length >= Qe)
      throw new Error(
        `Can't construct query with more than ${Qe} operators`
      );
    return r.operators.push({
      filter: d(t(Le))
    }), new e(r);
  }
  limit(t) {
    l(t, 1, "limit", "n");
    let r = this.takeQuery();
    return r.operators.push({ limit: t }), new e(r);
  }
  [Symbol.asyncIterator]() {
    return this.startQuery(), this;
  }
  async next() {
    (this.state.type === "closed" || this.state.type === "consumed") && Ge(this.state.type);
    let t = this.state.type === "preparing" ? this.startQuery() : this.state.queryId, { value: r, done: o } = await u("1.0/queryStreamNext", {
      queryId: t
    });
    return o && this.closeQuery(), { value: y(r), done: o };
  }
  return() {
    return this.closeQuery(), Promise.resolve({ done: !0, value: void 0 });
  }
  async paginate(t) {
    if (l(t, 1, "paginate", "options"), typeof t?.numItems != "number" || t.numItems < 0)
      throw new Error(
        `\`options.numItems\` must be a positive number. Received \`${t?.numItems}\`.`
      );
    let r = this.takeQuery(), o = t.numItems, s = t.cursor, a = t?.endCursor ?? null, i = t.maximumRowsRead ?? null, { page: f, isDone: p, continueCursor: N, splitCursor: dt, pageStatus: ht } = await u("1.0/queryPage", {
      query: r,
      cursor: s,
      endCursor: a,
      pageSize: o,
      maximumRowsRead: i,
      maximumBytesRead: t.maximumBytesRead,
      version: g
    });
    return {
      page: f.map((mt) => y(mt)),
      isDone: p,
      continueCursor: N,
      splitCursor: dt,
      pageStatus: ht
    };
  }
  async collect() {
    let t = [];
    for await (let r of this)
      t.push(r);
    return t;
  }
  async take(t) {
    return l(t, 1, "take", "n"), Je(t, 1, "take", "n"), this.limit(t).collect();
  }
  async first() {
    let t = await this.take(1);
    return t.length === 0 ? null : t[0];
  }
  async unique() {
    let t = await this.take(2);
    if (t.length === 0)
      return null;
    if (t.length === 2)
      throw new Error(`unique() query returned more than one result from table ${this.tableNameForErrorMessages}:
 [${t[0]._id}, ${t[1]._id}, ...]`);
    return t[0];
  }
};

// node_modules/convex/dist/esm/server/impl/database_impl.js
async function we(e, t, r) {
  if (l(t, 1, "get", "id"), typeof t != "string")
    throw new Error(
      `Invalid argument \`id\` for \`db.get\`, expected string but got '${typeof t}': ${t}`
    );
  let o = {
    id: m(t),
    isSystem: r,
    version: g,
    table: e
  }, s = await u("1.0/get", o);
  return y(s);
}
n(we, "get");
function Ae() {
  let e = /* @__PURE__ */ n((s = !1) => ({
    get: /* @__PURE__ */ n(async (a, i) => i !== void 0 ? await we(a, i, s) : await we(void 0, a, s), "get"),
    query: /* @__PURE__ */ n((a) => new J(a, s).query(), "query"),
    normalizeId: /* @__PURE__ */ n((a, i) => {
      l(a, 1, "normalizeId", "tableName"), l(i, 2, "normalizeId", "id");
      let f = a.startsWith("_");
      if (f !== s)
        throw new Error(
          `${f ? "System" : "User"} tables can only be accessed from db.${s ? "" : "system."}normalizeId().`
        );
      let p = B("1.0/db/normalizeId", {
        table: a,
        idString: i
      });
      return y(p).id;
    }, "normalizeId"),
    // We set the system reader on the next line
    system: null,
    table: /* @__PURE__ */ n((a) => new J(a, s), "table")
  }), "reader"), { system: t, ...r } = e(!0), o = e();
  return o.system = r, o;
}
n(Ae, "setupReader");
async function He(e, t) {
  if (e.startsWith("_"))
    throw new Error("System tables (prefixed with `_`) are read-only.");
  l(e, 1, "insert", "table"), l(t, 2, "insert", "value");
  let r = await u("1.0/insert", {
    table: e,
    value: m(t)
  });
  return y(r)._id;
}
n(He, "insert");
async function ge(e, t, r) {
  l(t, 1, "patch", "id"), l(r, 2, "patch", "value"), await u("1.0/shallowMerge", {
    id: m(t),
    value: qe(r),
    table: e
  });
}
n(ge, "patch");
async function xe(e, t, r) {
  l(t, 1, "replace", "id"), l(r, 2, "replace", "value"), await u("1.0/replace", {
    id: m(t),
    value: m(r),
    table: e
  });
}
n(xe, "replace");
async function be(e, t) {
  l(t, 1, "delete", "id"), await u("1.0/remove", {
    id: m(t),
    table: e
  });
}
n(be, "delete_");
function ze() {
  let e = Ae();
  return {
    get: e.get,
    query: e.query,
    normalizeId: e.normalizeId,
    system: e.system,
    insert: /* @__PURE__ */ n(async (t, r) => await He(t, r), "insert"),
    patch: /* @__PURE__ */ n(async (t, r, o) => o !== void 0 ? await ge(t, r, o) : await ge(void 0, t, r), "patch"),
    replace: /* @__PURE__ */ n(async (t, r, o) => o !== void 0 ? await xe(t, r, o) : await xe(void 0, t, r), "replace"),
    delete: /* @__PURE__ */ n(async (t, r) => r !== void 0 ? await be(t, r) : await be(void 0, t), "delete"),
    table: /* @__PURE__ */ n((t) => new ve(t, !1), "table")
  };
}
n(ze, "setupWriter");
var J = class {
  static {
    n(this, "TableReader");
  }
  constructor(t, r) {
    this.tableName = t, this.isSystem = r;
  }
  async get(t) {
    return we(this.tableName, t, this.isSystem);
  }
  query() {
    let t = this.tableName.startsWith("_");
    if (t !== this.isSystem)
      throw new Error(
        `${t ? "System" : "User"} tables can only be accessed from db.${this.isSystem ? "" : "system."}query().`
      );
    return new $(this.tableName);
  }
}, ve = class extends J {
  static {
    n(this, "TableWriter");
  }
  async insert(t) {
    return He(this.tableName, t);
  }
  async patch(t, r) {
    return ge(this.tableName, t, r);
  }
  async replace(t, r) {
    return xe(this.tableName, t, r);
  }
  async delete(t) {
    return be(this.tableName, t);
  }
};

// node_modules/convex/dist/esm/server/impl/scheduler_impl.js
function We() {
  return {
    runAfter: /* @__PURE__ */ n(async (e, t, r) => {
      let o = tr(e, t, r);
      return await u("1.0/schedule", o);
    }, "runAfter"),
    runAt: /* @__PURE__ */ n(async (e, t, r) => {
      let o = rr(
        e,
        t,
        r
      );
      return await u("1.0/schedule", o);
    }, "runAt"),
    cancel: /* @__PURE__ */ n(async (e) => {
      l(e, 1, "cancel", "id");
      let t = { id: m(e) };
      await u("1.0/cancel_job", t);
    }, "cancel")
  };
}
n(We, "setupMutationScheduler");
function tr(e, t, r) {
  if (typeof e != "number")
    throw new Error("`delayMs` must be a number");
  if (!isFinite(e))
    throw new Error("`delayMs` must be a finite number");
  if (e < 0)
    throw new Error("`delayMs` must be non-negative");
  let o = T(r), s = E(t), a = (Date.now() + e) / 1e3;
  return {
    ...s,
    ts: a,
    args: m(o),
    version: g
  };
}
n(tr, "runAfterSyscallArgs");
function rr(e, t, r) {
  let o;
  if (e instanceof Date)
    o = e.valueOf() / 1e3;
  else if (typeof e == "number")
    o = e / 1e3;
  else
    throw new Error("The invoke time must a Date or a timestamp");
  let s = E(t), a = T(r);
  return {
    ...s,
    ts: o,
    args: m(a),
    version: g
  };
}
n(rr, "runAtSyscallArgs");

// node_modules/convex/dist/esm/server/impl/storage_impl.js
function Ee(e) {
  return {
    getUrl: /* @__PURE__ */ n(async (t) => (l(t, 1, "getUrl", "storageId"), await u("1.0/storageGetUrl", {
      requestId: e,
      version: g,
      storageId: t
    })), "getUrl"),
    getMetadata: /* @__PURE__ */ n(async (t) => await u("1.0/storageGetMetadata", {
      requestId: e,
      version: g,
      storageId: t
    }), "getMetadata")
  };
}
n(Ee, "setupStorageReader");
function Ye(e) {
  let t = Ee(e);
  return {
    generateUploadUrl: /* @__PURE__ */ n(async () => await u("1.0/storageGenerateUploadUrl", {
      requestId: e,
      version: g
    }), "generateUploadUrl"),
    delete: /* @__PURE__ */ n(async (r) => {
      await u("1.0/storageDelete", {
        requestId: e,
        version: g,
        storageId: r
      });
    }, "delete"),
    getUrl: t.getUrl,
    getMetadata: t.getMetadata
  };
}
n(Ye, "setupStorageWriter");

// node_modules/convex/dist/esm/server/impl/meta_impl.js
async function Xe() {
  let e;
  try {
    e = await u("1.0/getTransactionMetrics", {});
  } catch (t) {
    throw t.message?.includes("Unknown async operation") ? new Error(
      "getTransactionMetrics() can only be called from a query or mutation. It is not available in actions or outside of a Convex function."
    ) : t;
  }
  return y(e);
}
n(Xe, "getTransactionMetrics");
async function Ke() {
  let { name: e, componentPath: t } = await u(
    "1.0/getFunctionMetadata",
    {}
  );
  return { name: e, componentPath: t };
}
n(Ke, "getFunctionMetadata");
async function Ze() {
  let e = await u(
    "1.0/getDeploymentMetadata",
    {}
  ), t = y(e);
  return {
    name: t.name,
    region: t.region ?? null,
    class: t.class
  };
}
n(Ze, "getDeploymentMetadata");
async function or() {
  let { ip: e, userAgent: t, requestId: r } = await u(
    "1.0/getRequestMetadata",
    {}
  );
  return { ip: e, userAgent: t, requestId: r };
}
n(or, "getRequestMetadata");
function et(e) {
  return {
    getFunctionMetadata: /* @__PURE__ */ n(async () => ({
      ...await Ke(),
      type: "query",
      visibility: e
    }), "getFunctionMetadata"),
    getTransactionMetrics: Xe,
    getDeploymentMetadata: Ze
  };
}
n(et, "setupQueryMeta");
function tt(e) {
  return {
    getFunctionMetadata: /* @__PURE__ */ n(async () => ({
      ...await Ke(),
      type: "mutation",
      visibility: e
    }), "getFunctionMetadata"),
    getTransactionMetrics: Xe,
    getDeploymentMetadata: Ze,
    getRequestMetadata: or
  };
}
n(tt, "setupMutationMeta");

// node_modules/convex/dist/esm/server/impl/registration_impl.js
async function sr(e, t, r) {
  let s = y(JSON.parse(t)), a = {
    db: ze(),
    auth: me(""),
    storage: Ye(""),
    scheduler: We(),
    meta: tt(r),
    runQuery: /* @__PURE__ */ n((f, p) => ie("query", f, p), "runQuery"),
    runSnapshotQuery: /* @__PURE__ */ n((f, p) => ie("snapshotQuery", f, p), "runSnapshotQuery"),
    runMutation: /* @__PURE__ */ n((f, p) => ie("mutation", f, p), "runMutation")
  }, i = await nt(e, a, s);
  return rt(i), JSON.stringify(m(i === void 0 ? null : i));
}
n(sr, "invokeMutation");
function rt(e) {
  if (e instanceof $ || e instanceof O)
    throw new Error(
      "Return value is a Query. Results must be retrieved with `.collect()`, `.take(n), `.unique()`, or `.first()`."
    );
}
n(rt, "validateReturnValue");
async function nt(e, t, r) {
  let o;
  try {
    o = await Promise.resolve(e(t, ...r));
  } catch (s) {
    throw ir(s);
  }
  return o;
}
n(nt, "invokeFunction");
function ot(e, t) {
  return (r, o) => (globalThis.console.warn(
    `Convex functions should not directly call other Convex functions. Consider calling a helper function instead. e.g. \`export const foo = ${e}(...); await foo(ctx);\` is not supported. See https://docs.convex.dev/production/best-practices/#use-helper-functions-to-write-shared-code`
  ), t(r, o));
}
n(ot, "dontCallDirectly");
function ir(e) {
  if (typeof e == "object" && e !== null && Symbol.for("ConvexError") in e) {
    let t = e;
    return t.data = JSON.stringify(
      m(t.data === void 0 ? null : t.data)
    ), t.ConvexErrorSymbol = Symbol.for("ConvexError"), t;
  } else
    return e;
}
n(ir, "serializeConvexErrorData");
function st() {
  if (typeof window > "u" || window.__convexAllowFunctionsInBrowser)
    return;
  (Object.getOwnPropertyDescriptor(globalThis, "window")?.get?.toString().includes("[native code]") ?? !1) && console.error(
    "Convex functions should not be imported in the browser. This will throw an error in future versions of `convex`. If this is a false negative, please report it to Convex support."
  );
}
n(st, "assertNotBrowser");
function it(e, t) {
  if (t === void 0)
    throw new Error(
      `A validator is undefined for field "${e}". This is often caused by circular imports. See https://docs.convex.dev/error#undefined-validator for details.`
    );
  return t;
}
n(it, "strictReplacer");
function at(e) {
  return () => {
    let t = c.any();
    return typeof e == "object" && e.args !== void 0 && (t = Z(e.args)), JSON.stringify(t.json, it);
  };
}
n(at, "exportArgs");
function ct(e) {
  return () => {
    let t;
    return typeof e == "object" && e.returns !== void 0 && (t = Z(e.returns)), JSON.stringify(t ? t.json : null, it);
  };
}
n(ct, "exportReturns");
var Se = /* @__PURE__ */ n(((e) => {
  let t = typeof e == "function" ? e : e.handler, r = ot("mutation", t);
  return st(), r.isMutation = !0, r.isPublic = !0, r.invokeMutation = (o) => sr(t, o, "public"), r.exportArgs = at(e), r.exportReturns = ct(e), r._handler = t, r;
}), "mutationGeneric");
async function ar(e, t, r) {
  let s = y(JSON.parse(t)), a = {
    db: Ae(),
    auth: me(""),
    storage: Ee(""),
    meta: et(r),
    runQuery: /* @__PURE__ */ n((f, p) => ie("query", f, p), "runQuery")
  }, i = await nt(e, a, s);
  return rt(i), JSON.stringify(m(i === void 0 ? null : i));
}
n(ar, "invokeQuery");
var Ie = /* @__PURE__ */ n(((e) => {
  let t = typeof e == "function" ? e : e.handler, r = ot("query", t);
  return st(), r.isQuery = !0, r.isPublic = !0, r.invokeQuery = (o) => ar(t, o, "public"), r.exportArgs = at(e), r.exportReturns = ct(e), r._handler = t, r;
}), "queryGeneric");
async function ie(e, t, r) {
  let o = T(r), s = {
    udfType: e,
    args: m(o),
    ...E(t)
  }, a = await u("1.0/runUdf", s);
  return y(a);
}
n(ie, "runUdf");

// node_modules/convex/dist/esm/server/pagination.js
var ns = c.object({
  numItems: c.number(),
  cursor: c.union(c.string(), c.null()),
  endCursor: c.optional(c.union(c.string(), c.null())),
  id: c.optional(c.number()),
  maximumRowsRead: c.optional(c.number()),
  maximumBytesRead: c.optional(c.number())
});

// node_modules/convex/dist/esm/server/api.js
function ut(e = []) {
  let t = {
    get(r, o) {
      if (typeof o == "string") {
        let s = [...e, o];
        return ut(s);
      } else if (o === U) {
        if (e.length < 2) {
          let i = ["api", ...e].join(".");
          throw new Error(
            `API path is expected to be of the form \`api.moduleName.functionName\`. Found: \`${i}\``
          );
        }
        let s = e.slice(0, -1).join("/"), a = e[e.length - 1];
        return a === "default" ? s : s + ":" + a;
      } else return o === Symbol.toStringTag ? "FunctionReference" : void 0;
    }
  };
  return new Proxy({}, t);
}
n(ut, "createApi");
var cr = ut();

// node_modules/convex/dist/esm/server/logVars.js
var lr = Symbol("var.requestId"), fr = Symbol("var.ip"), pr = Symbol("var.userAgent"), dr = Symbol("var.now"), hr = {
  [lr]: "requestId",
  [fr]: "ip",
  [pr]: "userAgent",
  [dr]: "now"
};

// node_modules/convex/dist/esm/server/schema.js
var mr = Object.defineProperty, yr = /* @__PURE__ */ n((e, t, r) => t in e ? mr(e, t, { enumerable: !0, configurable: !0, writable: !0, value: r }) : e[t] = r, "__defNormalProp"), S = /* @__PURE__ */ n((e, t, r) => yr(e, typeof t != "symbol" ? t + "" : t, r), "__publicField"), ae = class {
  static {
    n(this, "TableDefinition");
  }
  /**
   * @internal
   */
  constructor(t) {
    S(this, "indexes"), S(this, "stagedDbIndexes"), S(this, "searchIndexes"), S(this, "stagedSearchIndexes"), S(this, "vectorIndexes"), S(this, "stagedVectorIndexes"), S(this, "validator"), this.indexes = [], this.stagedDbIndexes = [], this.searchIndexes = [], this.stagedSearchIndexes = [], this.vectorIndexes = [], this.stagedVectorIndexes = [], this.validator = t;
  }
  /**
   * This API is experimental: it may change or disappear.
   *
   * Returns indexes defined on this table.
   * Intended for the advanced use cases of dynamically deciding which index to use for a query.
   * If you think you need this, please chime in on ths issue in the Convex JS GitHub repo.
   * https://github.com/get-convex/convex-js/issues/49
   */
  " indexes"() {
    return this.indexes;
  }
  index(t, r) {
    return Array.isArray(r) ? this.indexes.push({
      indexDescriptor: t,
      fields: r
    }) : r.staged ? this.stagedDbIndexes.push({
      indexDescriptor: t,
      fields: r.fields
    }) : this.indexes.push({
      indexDescriptor: t,
      fields: r.fields
    }), this;
  }
  searchIndex(t, r) {
    return r.staged ? this.stagedSearchIndexes.push({
      indexDescriptor: t,
      searchField: r.searchField,
      filterFields: r.filterFields || []
    }) : this.searchIndexes.push({
      indexDescriptor: t,
      searchField: r.searchField,
      filterFields: r.filterFields || []
    }), this;
  }
  vectorIndex(t, r) {
    return r.staged ? this.stagedVectorIndexes.push({
      indexDescriptor: t,
      vectorField: r.vectorField,
      dimensions: r.dimensions,
      filterFields: r.filterFields || []
    }) : this.vectorIndexes.push({
      indexDescriptor: t,
      vectorField: r.vectorField,
      dimensions: r.dimensions,
      filterFields: r.filterFields || []
    }), this;
  }
  /**
   * Work around for https://github.com/microsoft/TypeScript/issues/57035
   */
  self() {
    return this;
  }
  /**
   * Export the contents of this definition.
   *
   * This is called internally by the Convex framework.
   * @internal
   */
  export() {
    let t = this.validator.json;
    if (typeof t != "object")
      throw new Error(
        "Invalid validator: please make sure that the parameter of `defineTable` is valid (see https://docs.convex.dev/database/schemas)"
      );
    return {
      indexes: this.indexes,
      stagedDbIndexes: this.stagedDbIndexes,
      searchIndexes: this.searchIndexes,
      stagedSearchIndexes: this.stagedSearchIndexes,
      vectorIndexes: this.vectorIndexes,
      stagedVectorIndexes: this.stagedVectorIndexes,
      documentType: t
    };
  }
};
function Te(e) {
  return de(e) ? new ae(e) : new ae(c.object(e));
}
n(Te, "defineTable");
var _e = class {
  static {
    n(this, "SchemaDefinition");
  }
  /**
   * @internal
   */
  constructor(t, r) {
    S(this, "tables"), S(this, "strictTableNameTypes"), S(this, "schemaValidation"), this.tables = t, this.schemaValidation = r?.schemaValidation === void 0 ? !0 : r.schemaValidation;
  }
  /**
   * Export the contents of this definition.
   *
   * This is called internally by the Convex framework.
   * @internal
   */
  export() {
    return JSON.stringify({
      tables: Object.entries(this.tables).map(([t, r]) => {
        let {
          indexes: o,
          stagedDbIndexes: s,
          searchIndexes: a,
          stagedSearchIndexes: i,
          vectorIndexes: f,
          stagedVectorIndexes: p,
          documentType: N
        } = r.export();
        return {
          tableName: t,
          indexes: o,
          stagedDbIndexes: s,
          searchIndexes: a,
          stagedSearchIndexes: i,
          vectorIndexes: f,
          stagedVectorIndexes: p,
          documentType: N
        };
      }),
      schemaValidation: this.schemaValidation
    });
  }
};
function lt(e, t) {
  return new _e(e, t);
}
n(lt, "defineSchema");
var Ms = lt({
  _scheduled_functions: Te({
    name: c.string(),
    args: c.array(c.any()),
    scheduledTime: c.float64(),
    completedTime: c.optional(c.float64()),
    state: c.union(
      c.object({ kind: c.literal("pending") }),
      c.object({ kind: c.literal("inProgress") }),
      c.object({ kind: c.literal("success") }),
      c.object({ kind: c.literal("failed"), error: c.string() }),
      c.object({ kind: c.literal("canceled") })
    )
  }),
  _storage: Te({
    sha256: c.string(),
    size: c.float64(),
    contentType: c.optional(c.string())
  })
});

// convex/_generated/server.js
var ft = Ie;
var pt = Se;

// convex/messages.ts
var yi = pt({
  args: {},
  handler: /* @__PURE__ */ n(async (e) => await e.db.insert("messages", { name: "ian", body: "hello" }), "handler")
}), wi = ft({
  args: { id: c.id("messages") },
  handler: /* @__PURE__ */ n(async (e, { id: t }) => await e.db.get(t), "handler")
});
export {
  wi as getById,
  yi as seedIan
};
//# sourceMappingURL=messages.js.map
