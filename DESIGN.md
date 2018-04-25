# Design of `wasm-bindgen`

This is intended to be a bit of a deep-dive into how `wasm-bindgen` works today,
specifically for Rust. If you're reading this far in the future it may no longer
be up to date, but feel free to ping me and I can try to answer questions and/or
update this!

## Foundation: ES Modules

The first thing to know about `wasm-bindgen` is that it's fundamentally built on
the idea of ES Modules. In other words this tool takes an opinionated stance
that wasm files *should be viewed as ES6 modules*. This means that you can
`import` from a wasm file, use its `export`-ed functionality, etc, from normal
JS files.

Now unfortunately at the time of this writing the interface of wasm interop
isn't very rich. Wasm modules can only call functions or export functions that
deal exclusively with `i32`, `i64`, `f32`, and `f64`. Bummer!

That's where this project comes in. The goal of `wasm-bindgen` is to enhance the
"ABI" of wasm modules with richer types like classes, JS objects, Rust structs,
strings, etc. Keep in mind, though, that everything is based on ES Modules! This
means that the compiler is actually producing a "broken" wasm file of sorts. The
wasm file emitted by rustc, for example, does not have the interface we would
like to have. Instead it requires the `wasm-bindgen` tool to postprocess the
file, generating a `foo.js` and `foo_bg.wasm` file. The `foo.js` file is the
desired interface expressed in JS (classes, types, strings, etc) and the
`foo_bg.wasm` module is simply used as an implementation detail (it was
lightly modified from the original `foo.wasm` file).

## Foundation #2: Unintrusive in Rust

On the more Rust-y side of things the `wasm-bindgen` crate is designed to
ideally have as minimal impact on a Rust crate as possible. Ideally a few
`#[wasm_bindgen]` attributes are annotated in key locations and otherwise you're
off to the races, but otherwise it strives to both not invent new syntax and
work with existing idioms today.

For example the `#[no_mangle]` and `extern` ABI indicators are required for
annotated free functions with `#[wasm_bindgen]`, because these two snippets are
actually equivalent:

```rust
#[no_mangle]
pub extern fn only_integers(a: i32) -> u32 {
    // ...
}

// is equivalent to...

#[wasm_bindgen]
pub fn only_integers_with_wasm_bindgen(a: i32) -> u32 {
    // ...
}
```

Additionally the design here with minimal intervention in Rust should allow us
to easily take advantage of the upcoming [host bindings][host] proposal. Ideally
you'd simply upgrade `wasm-bindgen`-the-crate as well as your toolchain and
you're immediately getting raw access to host bindings! (this is still a bit of
a ways off though...)

[host]: https://github.com/WebAssembly/host-bindings

## Polyfill for "JS objects in wasm"

One of the main goals of `wasm-bindgen` is to allow working with and passing
around JS objects in wasm. But wait, that's not allowed today! While indeed
true, that's where the polyfill comes in!

The question here is how we shoehorn JS objects into a `u32` for wasm to use.
The current strategy for this approach is to maintain two module-local variables
in the generated `foo.js` file: a stack and a heap.

### Temporary JS objects on the stack

The stack in `foo.js` is, well, a stack. JS objects are pushed on the top of the
stack, and their index in the stack is the identifier that's passed to wasm. JS
objects are then only removed from the top of the stack as well. This data
structure is mainly useful for efficiently passing a JS object into wasm without
a sort of "heap allocation". The downside of this, however, is that it only
works for when wasm doesn't hold onto a JS object (aka it only gets a
"reference" in Rust parlance).

Let's take a look at an example.

```rust
// foo.rs
#[wasm_bindgen]
pub fn foo(a: &JsValue) {
    // ...
}
```

Here we're using the special `JsValue` type from the `wasm-bindgen` library
itself. Our exported function, `foo`, takes a *reference* to an object. This
notably means that it can't persist the object past the lifetime of this
function call.

Now what we actually want to generate is a JS module that looks like (in
Typescript parlance)

```ts
// foo.d.ts
export function foo(a: any);
```

and what we actually generate looks something like:

```js
// foo.js
import * as wasm from './foo_bg';

let stack = [];

function addBorrowedObject(obj) {
  stack.push(obj);
  return stack.length - 1;
}

export function foo(arg0) {
  const idx0 = addBorrowedObject(arg0);
  try {
    wasm.foo(idx0);
  } finally {
    stack.pop();
  }
}
```

Here we can see a few notable points of action:

* The wasm file was renamed to `foo_bg.wasm`, and we can see how the JS module
  generated here is importing from the wasm file.
* Next we can see our `stack` module variable which is used to push/pop items
  from the stack.
* Our exported function `foo`, takes an arbitrary argument, `arg0`, which is
  converted to an index with the `addBorrowedObject` object function. The index
  is then passed to wasm so wasm can operate with it.
* Finally, we have a `finally` which frees the stack slot as it's no longer
  used, issuing a `pop` for what was pushed at the start of the function.

It's also helpful to dig into the Rust side of things to see what's going on
there! Let's take a look at the code that `#[wasm_bindgen]` generates in Rust:

```rust
// what the user wrote, note that #[no_mangle] is removed
pub extern fn foo(a: &JsValue) {
    // ...
}

#[export_name = "foo"]
pub extern fn __wasm_bindgen_generated_foo(arg0: u32) {
    let arg0 = unsafe {
        ManuallyDrop::new(JsValue::__from_idx(arg0))
    };
    let arg0 = &*arg0;
    foo(arg0);
}
```

And as with the JS, the notable points here are:

* The original function, `foo`, is unmodified in the output
* A generated function here (with a unique name) is the one that's actually
  exported from the wasm module
* Our generated function takes an integer argument (our index) and then wraps it
  in a `JsValue`. There's some trickery here that's not worth going into just
  yet, but we'll see in a bit what's happening under the hood.

### Long-lived JS objects in a slab

The above strategy is useful when JS objects are only temporarily used in Rust,
for example only during one function call. Sometimes, though, objects may have a
dynamic lifetime or otherwise need to be stored on Rust's heap. To cope with
this there's a second half of management of JS objects, a slab.

JS Objects passed to wasm that are not references are assumed to have a dynamic
lifetime inside of the wasm module. As a result the strict push/pop of the stack
won't work and we need more permanent storage for the JS objects. To cope with
this we build our own "slab allocator" of sorts.

A picture (or code) is worth a thousand words so let's show what happens with an
example.

```rust
// foo.rs
#[wasm_bindgen]
pub fn foo(a: JsValue) {
    // ...
}
```

Note that the `&` is missing in front of the `JsValue` we had before, and in
Rust parlance this means it's taking ownership of the JS value. The exported ES
module interface is the same as before, but the ownership mechanics are slightly
different. Let's see the generated JS's slab in action:

```js
import * as wasm from './foo_bg'; // imports from wasm file

let slab = [];
let slab_next = 0;

function addHeapObject(obj) {
  if (slab_next === slab.length)
    slab.push(slab.length + 1);
  const idx = slab_next;
  const next = slab[idx];
  slab_next = next;
  slab[idx] = { obj, cnt: 1 };
  return idx;
}

export function foo(arg0) {
  const idx0 = addHeapObject(arg0);
  wasm.foo(idx0);
}

export function __wbindgen_object_drop_ref(idx) {
  let obj = slab[idx];
  obj.cnt -= 1;
  if (obj.cnt > 0)
    return;
  // If we hit 0 then free up our space in the slab
  slab[idx] = slab_next;
  slab_next = idx;
}
```

Unlike before we're now calling `addHeapObject` on the argument to `foo` rather
than `addBorrowedObject`. This function will use `slab` and `slab_next` as a
slab allocator to acquire a slot to store the object, placing a structure there
once it's found.

Note here that a reference count is used in addition to storing the object.
That's so we can create multiple references to the JS object in Rust without
using `Rc`, but it's overall not too important to worry about here.

Another curious aspect of this generated module is the
`__wbindgen_object_drop_ref` function. This is one that's actually imported from
wasm rather than used in this module! This function is used to signal the end of
the lifetime of a `JsValue` in Rust, or in other words when it goes out of
scope. Otherwise though this function is largely just a general "slab free"
implementation.

And finally, let's take a look at the Rust generated again too:

```rust
// what the user wrote
pub extern fn foo(a: JsValue) {
    // ...
}

#[export_name = "foo"]
pub extern fn __wasm_bindgen_generated_foo(arg0: u32) {
    let arg0 = unsafe {
        JsValue::__from_idx(arg0)
    };
    foo(arg0);
}
```

Ah that looks much more familiar! Not much interesting is happening here, so
let's move on to...

### Anatomy of `JsValue`

Currently the `JsValue` struct is actually quite simple in Rust, it's:

```rust
pub struct JsValue {
    idx: u32,
}

// "private" constructors

impl Drop for JsValue {
    fn drop(&mut self) {
        unsafe {
            __wbindgen_object_drop_ref(self.idx);
        }
    }
}
```

Or in other words it's a newtype wrapper around a `u32`, the index that we're
passed from wasm. The destructor here is where the `__wbindgen_object_drop_ref`
function is called to relinquish our reference count of the JS object, freeing
up our slot in the `slab` that we saw above.

If you'll recall as well, when we took `&JsValue` above we generated a wrapper
of `ManuallyDrop` around the local binding, and that's because we wanted to
avoid invoking this destructor when the object comes from the stack.

### Indexing both a slab and the stack

You might be thinking at this point that this system may not work! There's
indexes into both the slab and the stack mixed up, but how do we differentiate?
It turns out that the examples above have been simplified a bit, but otherwise
the lowest bit is currently used as an indicator of whether you're a slab or a
stack index.

## Exporting a function to JS

Alright now that we've got a good grasp on JS objects and how they're working,
let's take a look at another feature of `wasm-bindgen`: exporting functionality
with types that are richer than just numbers.

The basic idea around exporting functionality with more flavorful types is that
the wasm exports won't actually be called directly. Instead the generated
`foo.js` module will have shims for all exported functions in the wasm module.

The most interesting conversion here happens with strings so let's take a look
at that.

```rust
#[wasm_bindgen]
pub fn greet(a: &str) -> String {
    format!("Hello, {}!", a)
}
```

Here we'd like to define an ES module that looks like

```ts
// foo.d.ts
export function greet(a: string): string;
```

To see what's going on, let's take a look at the generated shim

```js
import * as wasm from './foo_bg';

function passStringToWasm(arg) {
  const buf = new TextEncoder('utf-8').encode(arg);
  const len = buf.length;
  const ptr = wasm.__wbindgen_malloc(len);
  let array = new Uint8Array(wasm.memory.buffer);
  array.set(buf, ptr);
  return [ptr, len];
}

function getStringFromWasm(ptr, len) {
  const mem = new Uint8Array(wasm.memory.buffer);
  const slice = mem.slice(ptr, ptr + len);
  const ret = new TextDecoder('utf-8').decode(slice);
  return ret;
}

export function greet(arg0) {
  const [ptr0, len0] = passStringToWasm(arg0);
  try {
    const ret = wasm.greet(ptr0, len0);
    const ptr = wasm.__wbindgen_boxed_str_ptr(ret);
    const len = wasm.__wbindgen_boxed_str_len(ret);
    const realRet = getStringFromWasm(ptr, len);
    wasm.__wbindgen_boxed_str_free(ret);
    return realRet;
  } finally {
    wasm.__wbindgen_free(ptr0, len0);
  }
}
```

Phew, that's quite a lot! We can sort of see though if we look closely what's
happening:

* Strings are passed to wasm via two arguments, a pointer and a length. Right
  now we have to copy the string onto the wasm heap which means we'll be using
  `TextEncoder` to actually do the encoding. Once this is done we use an
  internal function in `wasm-bindgen` to allocate space for the string to go,
  and then we'll pass that ptr/length to wasm later on.

* Returning strings from wasm is a little tricky as we need to return a ptr/len
  pair, but wasm currently only supports one return value (multiple return values
  [is being standardized](https://github.com/WebAssembly/design/issues/1146)).
  To work around this in the meantime, we're actually returning a pointer to a
  ptr/len pair, and then using functions to access the various fields.

* Some cleanup ends up happening in wasm. The `__wbindgen_boxed_str_free`
  function is used to free the return value of `greet` after it's been decoded
  onto the JS heap (using `TextDecoder`). The `__wbindgen_free` is then used to
  free the space we allocated to pass the string argument once the function call
  is done.

Next let's take a look at the Rust side of things as well. Here we'll be looking
at a mostly abbreviated and/or "simplified" in the sense of this is what it
compiles down to:

```rust
pub extern fn greet(a: &str) -> String {
    format!("Hello, {}!", a)
}

#[export_name = "greet"]
pub extern fn __wasm_bindgen_generated_greet(
    arg0_ptr: *const u8,
    arg0_len: usize,
) -> *mut String {
    let arg0 = unsafe {
        let slice = ::std::slice::from_raw_parts(arg0_ptr, arg0_len);
        ::std::str::from_utf8_unchecked(slice)
    };
    let _ret = greet(arg0);
    Box::into_raw(Box::new(_ret))
}
```

Here we can see again that our `greet` function is unmodified and has a wrapper
to call it. This wrapper will take the ptr/len argument and convert it to a
string slice, while the return value is boxed up into just a pointer and is
then returned up to was for reading via the `__wbindgen_boxed_str_*` functions.

So in general exporting a function involves a shim both in JS and in Rust with
each side translating to or from wasm arguments to the native types of each
language. The `wasm-bindgen` tool manages hooking up all these shims while the
`#[wasm_bindgen]` macro takes care of the Rust shim as well.

Most arguments have a relatively clear way to convert them, bit if you've got
any questions just let me know!

## Importing a function from JS

Now that we've exported some rich functionality to JS it's also time to import
some! The goal here is to basically implement JS `import` statements in Rust,
with fancy types and all.

First up, let's say we invert the function above and instead want to generate
greetings in JS but call it from Rust. We might have, for example:

```rust
#[wasm_bindgen(module = "./greet")]
extern {
    fn greet(a: &str) -> String;
}

fn other_code() {
    let greeting = greet("foo");
    // ...
}
```

The basic idea of imports is the same as exports in that we'll have shims in
both JS and Rust doing the necessary translation. Let's first see the JS shim in
action:

```js
import * as wasm from './foo_bg';

import { greet } from './greet';

// ...

export function __wbg_f_greet(ptr0, len0, wasmretptr) {
  const [retptr, retlen] = passStringToWasm(greet(getStringFromWasm(ptr0, len0)));
  (new Uint32Array(wasm.memory.buffer))[wasmretptr / 4] = retlen;
  return retptr;
}
```

The `getStringFromWasm` and `passStringToWasm` are the same as we saw before,
and like with `__wbindgen_object_drop_ref` far above we've got this weird export
from our module now! The `__wbg_f_greet` function is what's generated by
`wasm-bindgen` to actually get imported in the `foo.wasm` module.

The generated `foo.js` we see imports from the `./greet` module with the `greet`
name (was the function import in Rust said) and then the `__wbg_f_greet`
function is shimming that import.

There's some tricky ABI business going on here so let's take a look at the
generated Rust as well. Like before this is simplified from what's actually
generated.

```rust
extern fn greet(a: &str) -> String {
    extern {
        fn __wbg_f_greet(a_ptr: *const u8, a_len: usize, ret_len: *mut usize) -> *mut u8;
    }
    unsafe {
        let a_ptr = a.as_ptr();
        let a_len = a.len();
        let mut __ret_strlen = 0;
        let mut __ret_strlen_ptr = &mut __ret_strlen as *mut usize;
        let _ret = __wbg_f_greet(a_ptr, a_len, __ret_strlen_ptr);
        String::from_utf8_unchecked(
            Vec::from_raw_parts(_ret, __ret_strlen, __ret_strlen)
        )
    }
}
```

Here we can see that the `greet` function was generated but it's largely just a
shim around the `__wbg_f_greet` function that we're calling. The ptr/len pair
for the argument is passed as two arguments and for the return value we're
receiving one value (the length) indirectly while directly receiving the
returned pointer.

## Exporting a struct to JS

So far we've covered JS objects, importing functions, and exporting functions.
This has given us quite a rich base to build on so far, and that's great! We
sometimes, though, want to go even further and define a JS `class` in Rust. Or
in other words, we want to expose an object with methods from Rust to JS rather
than just importing/exporting free functions.

The `#[wasm_bindgen]` attribute can annotate both a `struct` and `impl` blocks
to allow:

```rust
#[wasm_bindgen]
pub struct Foo {
    internal: i32,
}

#[wasm_bindgen]
impl Foo {
    pub fn new(val: i32) -> Foo {
        Foo { internal: val }
    }

    pub fn get(&self) -> i32 {
        self.internal
    }

    pub fn set(&mut self, val: i32) {
        self.internal = val;
    }
}
```

This is a typical Rust `struct` definition for a type with a constructor and a
few methods. Annotating the struct with `#[wasm_bindgen]` means that we'll
generate necessary trait impls to convert this type to/from the JS boundary. The
annotated `impl` block here means that the functions inside will also be made
available to JS through generated shims. If we take a look at the generated JS
code for this we'll see:

```js
import * as wasm from './js_hello_world_bg';

export class Foo {
    static __construct(ptr) {
        return new Foo(ptr);
    }

    constructor(ptr) {
        this.ptr = ptr;
    }

    free() {
        const ptr = this.ptr;
        this.ptr = 0;
        wasm.__wbg_foo_free(ptr);
    }

    static new(arg0) {
        const ret = wasm.foo_new(arg0);
        return Foo.__construct(ret)
    }

    get() {
        const ret = wasm.foo_get(this.ptr);
        return ret;
    }

    set(arg0) {
        const ret = wasm.foo_set(this.ptr, arg0);
        return ret;
    }
}
```

That's actually not much! We can see here though how we've translated from Rust
to JS:

* Associated functions in Rust (those without `self`) turn into `static`
  functions in JS.
* Methods in Rust turn into methods in wasm.
* Manual memory management is exposed in JS as well. The `free` function is
  required to be invoked to deallocate resources on the Rust side of things.

To be able to use `new Foo()`, you'd need to annotate `new` as `#[wasm_bindgen(constructor)]`.

One important aspect to note here, though, is that once `free` is called the JS
object is "neutered" in that its internal pointer is nulled out. This means that
future usage of this object should trigger a panic in Rust.

The real trickery with these bindings ends up happening in Rust, however, so
let's take a look at that.

```rust
// original input to `#[wasm_bindgen]` omitted ...

#[export_name = "foo_new"]
pub extern fn __wasm_bindgen_generated_Foo_new(arg0: i32) -> u32
    let ret = Foo::new(arg0);
    Box::into_raw(Box::new(WasmRefCell::new(ret))) as u32
}

#[export_name = "foo_get"]
pub extern fn __wasm_bindgen_generated_Foo_get(me: u32) -> i32 {
    let me = me as *mut WasmRefCell<Foo>;
    wasm_bindgen::__rt::assert_not_null(me);
    let me = unsafe { &*me };
    return me.borrow().get();
}

#[export_name = "foo_set"]
pub extern fn __wasm_bindgen_generated_Foo_set(me: u32, arg1: i32) {
    let me = me as *mut WasmRefCell<Foo>;
    ::wasm_bindgen::__rt::assert_not_null(me);
    let me = unsafe { &*me };
    me.borrow_mut().set(arg1);
}

#[no_mangle]
pub unsafe extern fn __wbindgen_foo_free(me: u32) {
    let me = me as *mut WasmRefCell<Foo>;
    wasm_bindgen::__rt::assert_not_null(me);
    (*me).borrow_mut(); // ensure no active borrows
    drop(Box::from_raw(me));
}
```

As with before this is cleaned up from the actual output but it's the same idea
as to what's going on! Here we can see a shim for each function as well as a
shim for deallocating an instance of `Foo`. Recall that the only valid wasm
types today are numbers, so we're required to shoehorn all of `Foo` into a
`u32`, which is currently done via `Box` (like `std::unique_ptr` in C++).
Note, though, that there's an extra layer here, `WasmRefCell`. This type is the
same as [`RefCell`] and can be mostly glossed over.

The purpose for this type, if you're interested though, is to uphold Rust's
guarantees about aliasing in a world where aliasing is rampant (JS).
Specifically the `&Foo` type means that there can be as much alaising as you'd
like, but crucially `&mut Foo` means that it is the sole pointer to the data
(no other `&Foo` to the same instance exists). The [`RefCell`] type in libstd
is a way of dynamically enforcing this at runtime (as opposed to compile time
where it usually happens). Baking in `WasmRefCell` is the same idea here,
adding runtime checks for aliasing which are typically happening at compile
time. This is currently a Rust-specific feature which isn't actually in the
`wasm-bindgen` tool itself, it's just in the Rust-generated code (aka the
`#[wasm_bindgen]` attribute).

[`RefCell`]: https://doc.rust-lang.org/std/cell/struct.RefCell.html

## Importing a class from JS

Just like with functions after we've started exporting we'll also want to
import! Now that we've exported a `class` to JS we'll want to also be able to
import classes in Rust as well to invoke methods and such. Since JS classes are
in general just JS objects the bindings here will look pretty similar to the JS
object bindings describe above.

As usual though, let's dive into an example!

```rust
#[wasm_bindgen(module = "./bar")]
extern {
    type Bar;

    #[wasm_bindgen(constructor)]
    fn new(arg: i32) -> Bar;

    #[wasm_bindgen(js_namespace = Bar)]
    fn another_function() -> i32;

    #[wasm_bindgen(method)]
    fn get(this: &Bar) -> i32;

    #[wasm_bindgen(method)]
    fn set(this: &Bar, val: i32);

    #[wasm_bindgen(method, getter)]
    fn property(this: &Bar) -> i32;

    #[wasm_bindgen(method, setter)]
    fn set_property(this: &Bar, val: i32);
}

fn run() {
    let bar = Bar::new(Bar::another_function());
    let x = bar.get();
    bar.set(x + 3);

    bar.set_property(bar.property() + 6);
}
```

Unlike our previous imports, this one's a bit more chatty! Remember that one of
the goals of `wasm-bindgen` is to use native Rust syntax wherever possible, so
this is mostly intended to use the `#[wasm_bindgen]` attribute to interpret
what's written down in Rust. Now there's a few attribute annotations here, so
let's go through one-by-one:

* `#[wasm_bindgen(module = "./bar")]` - seen before with imports this is declare
  where all the subsequent functionality is imported form. For example the `Bar`
  type is going to be imported from the `./bar` module.
* `type Bar` - this is a declaration of JS class as a new type in Rust. This
  means that a new type `Bar` is generated which is "opaque" but is represented
  as internally containing a `JsValue`. We'll see more on this later.
* `#[wasm_bindgen(constructor)]` - this indicates that the binding's name isn't
  actually used in JS but rather translates to `new Bar()`. The return value of
  this function must be a bare type, like `Bar`.
* `#[wasm_bindgen(js_namespace = Bar)]` - this attribute indicates that the
  function declaration is namespaced through the `Bar` class in JS.
* `#[wasm_bindgen(method)]` - and finally, this attribute indicates that a
  method call is going to happen. The first argument must be a JS struct, like
  `Bar`, and the call in JS looks like `Bar.prototype.set.call(...)`.

With all that in mind, let's take a look at the JS generated.

```js
import * as wasm from './foo_bg';

import { Bar } from './bar';

// other support functions omitted...

export function __wbg_s_Bar_new() {
  return addHeapObject(new Bar());
}

const another_function_shim = Bar.another_function;
export function __wbg_s_Bar_another_function() {
  return another_function_shim();
}

const get_shim = Bar.prototype.get;
export function __wbg_s_Bar_get(ptr) {
  return shim.call(getObject(ptr));
}

const set_shim = Bar.prototype.set;
export function __wbg_s_Bar_set(ptr, arg0) {
  set_shim.call(getObject(ptr), arg0)
}

const property_shim = Object.getOwnPropertyDescriptor(Bar.prototype, 'property').get;
export function __wbg_s_Bar_property(ptr) {
  return property_shim.call(getObject(ptr));
}

const set_property_shim = Object.getOwnPropertyDescriptor(Bar.prototype, 'property').set;
export function __wbg_s_Bar_set_property(ptr, arg0) {
  set_property_shim.call(getObject(ptr), arg0)
}
```

Like when importing functions from JS we can see a bunch of shims are generated
for all the relevant functions. The `new` static function has the
`#[wasm_bindgen(constructor)]` attribute which means that instead of any
particular method it should actually invoke the `new` constructor instead (as
we see here). The static function `another_function`, however, is dispatched as
`Bar.another_function`.

The `get` and `set` functions are methods so they go through `Bar.prototype`,
and otherwise their first argument is implicitly the JS object itself which is
loaded through `getObject` like we saw earlier.

Some real meat starts to show up though on the Rust side of things, so let's
take a look:

```rust
pub struct Bar {
    obj: JsValue,
}

impl Bar {
    fn new() -> Bar {
        extern {
            fn __wbg_s_Bar_new() -> u32;
        }
        unsafe {
            let ret = __wbg_s_Bar_new();
            Bar { obj: JsValue::__from_idx(ret) }
        }
    }

    fn another_function() -> i32 {
        extern {
            fn __wbg_s_Bar_another_function() -> i32;
        }
        unsafe {
            __wbg_s_Bar_another_function()
        }
    }

    fn get(&self) -> i32 {
        extern {
            fn __wbg_s_Bar_get(ptr: u32) -> i32;
        }
        unsafe {
            let ptr = self.obj.__get_idx();
            let ret = __wbg_s_Bar_get(ptr);
            return ret
        }
    }

    fn set(&self, val: i32) {
        extern {
            fn __wbg_s_Bar_set(ptr: u32, val: i32);
        }
        unsafe {
            let ptr = self.obj.__get_idx();
            __wbg_s_Bar_set(ptr, val);
        }
    }

    fn property(&self) -> i32 {
        extern {
            fn __wbg_s_Bar_property(ptr: u32) -> i32;
        }
        unsafe {
            let ptr = self.obj.__get_idx();
            let ret = __wbg_s_Bar_property(ptr);
            return ret
        }
    }

    fn set_property(&self, val: i32) {
        extern {
            fn __wbg_s_Bar_set_property(ptr: u32, val: i32);
        }
        unsafe {
            let ptr = self.obj.__get_idx();
            __wbg_s_Bar_set_property(ptr, val);
        }
    }
}

impl WasmBoundary for Bar {
    // ...
}

impl ToRefWasmBoundary for Bar {
    // ...
}
```

In Rust we're seeing that a new type, `Bar`, is generated for this import of a
class. The type `Bar` internally contains a `JsValue` as an instance of `Bar`
is meant to represent a JS object stored in our module's stack/slab. This then
works mostly the same way that we saw JS objects work in the beginning.

When calling `Bar::new` we'll get an index back which is wrapped up in `Bar`
(which is itself just a `u32` in memory when stripped down). Each function then
passes the index as the first argument and otherwise forwards everything along
in Rust.

## Imports and JS exceptions

By default `wasm-bindgen` will take no action when wasm calls a JS function
which ends up throwing an exception. The wasm spec right now doesn't support
stack unwinding and as a result Rust code **will not execute destructors**. This
can unfortunately cause memory leaks in Rust right now, but as soon as wasm
implements catching exceptions we'll be sure to add support as well!

In the meantime though fear not! You can, if necessary, annotate some imports
as whether they should catch an exception. For example:

```rust
#[wasm_bindgen(module = "./bar")]
extern {
    #[wasm_bindgen(catch)]
    fn foo() -> Result<(), JsValue>;
}
```

Here the import of `foo` is annotated that it should catch the JS exception, if
one occurs, and return it to wasm. This is expressed in Rust with a `Result`
type where the `T` of the result is the otherwise successful result of the
function, and the `E` *must* be `JsValue`.

Under the hood this generates shims that do a bunch of translation, but it
suffices to say that a call in wasm to `foo` should always return
appropriately.


## Customizing import behavior

The `#[wasm_bindgen]` macro supports a good amount of configuration for
controlling precisely how imports are imported and what they map to in JS. This
section is intended to hopefully be an exhaustive reference of the
possibilities!

* `module` and `version` - we've seen `module` so far indicating where we can
  import items from but `version` is also allowed:

  ```rust
  #[wasm_bindgen(module = "moment", version = "2.0.0")]
  extern {
      type Moment;
      fn moment() -> Moment;
      #[wasm_bindgen(method)]
      fn format(this: &Moment) -> String;
  }
  ```

  The `module` key is used to configure the module that each item is imported
  from. The `version` key does not affect the generated wasm itself but rather
  it's an informative directive for tools like [wasm-pack]. Tools like wasm-pack
  will generate a `package.json` for you and the `version` listed here, when
  `module` is also an NPM package, will correspond to what to write down in
  `package.json`.

  In other words the usage of `module` as the name of an NPM package and
  `version` as the version requirement allows you to, inline in Rust, depend on
  the NPM ecosystem and import functionality from those packages. When bundled
  with a tool like [wasm-pack] everything will automatically get wired up with
  bundlers and you should be good to go!

[wasm-pack]: https://github.com/ashleygwilliams/wasm-pack

* `catch` - as we saw before the `catch` attribute allows catching a JS
  exception. This can be attached to any imported function and the function must
  return a `Result` where the `Err` payload is a `JsValue`, like so:

  ```rust
  #[wasm_bindgen]
  extern {
      #[wasm_bindgen(catch)]
      fn foo() -> Result<(), JsValue>;
  }
  ```

  If the imported function throws an exception then `Err` will be returned with
  the exception that was raised, and otherwise `Ok` is returned with the result
  of the function.

* `constructor` - this is used to indicate that the function being bound should
  actually translate to a `new` constructor in JS. The final argument must be a
  type that's imported from JS, and it's what'll get used in JS:

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(constructor)]
      fn new() -> Foo;
  }
  ```

  This will attach the `new` function to the `Foo` type (implied by
  `constructor`) and in JS when this function is called it will be equivalent to
  `new Foo()`.

* `method` - this is the gateway to adding methods to imported objects or
  otherwise accessing properties on objects via methods and such. This should be
  done for doing the equivalent of expressions like `foo.bar()` in JS.

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(method)]
      fn work(this: &Foo);
  }
  ```

  The first argument of a `method` annotation must be a borrowed reference (not
  mutable, shared) to the type that the method is attached to. In this case
  we'll be able to call this method like `foo.work()` in JS (where `foo` has
  type `Foo`).

  In JS this invocation will correspond to accessing `Foo.prototype.work` and
  then calling that when the import is called. Note that `method` by default
  implies going through `prototype` to get a function pointer.

* `js_namespace` - this attribute indicates that the JS type is accessed through
  a particular namespace. For example the `WebAssembly.Module` APIs are all
  accessed through the `WebAssembly` namespace. The `js_namespace` can be
  applied to any import and whenever the generated JS attempts to reference a
  name (like a class or function name) it'll be accessed through this namespace.

  ```rust
  #[wasm_bindgen]
  extern {
      #[wasm_bindgen(js_namespace = console)]
      fn log(s: &str);
  }
  ```

  This is an example of how to bind `console.log(x)` in Rust. The `log` function
  will be available in the Rust module and will be invoked as `console.log` in
  JS.

* `getter` and `setter` - these two attributes can be combined with `method` to
  indicate that this is a getter or setter method. A `getter`-tagged function by
  default accesses the JS property with the same name as the getter function. A
  `setter`'s function name is currently required to start with "set\_" and the
  property it accesses is the suffix after "set\_".

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(method, getter)]
      fn property(this: &Foo) -> u32;
      #[wasm_bindgen(method, setter)]
      fn set_property(this: &Foo, val: u32);
  }
  ```

  Here we're importing the `Foo` type and defining the ability to access each
  object's `property` property. The first function here is a getter and will be
  available in Rust as `foo.property()`, and the latter is the setter which is
  accessible as `foo.set_property(2)`. Note that both functions have a `this`
  argument as they're tagged with `method`.

  Finally, you can also pass an argument to the `getter` and `setter`
  properties to configure what property is accessed. When the property is
  explicitly specified then there is no restriction on the method name. For
  example the below is equivalent to the above:

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(method, getter = property)]
      fn assorted_method_name(this: &Foo) -> u32;
      #[wasm_bindgen(method, setter = "property")]
      fn some_other_method_name(this: &Foo, val: u32);
  }
  ```

  Properties in JS are accessed through `Object.getOwnPropertyDescriptor`. Note
  that this typically only works for class-like-defined properties which aren't
  just attached properties on any old object. For accessing any old property on
  an object we can use...

* `structural` - this is a flag to `method` annotations which indicates that the
  method being accessed (or property with getters/setters) should be accessed in
  a structural fashion. For example methods are *not* accessed through
  `prototype` and properties are accessed on the object directly rather than
  through `Object.getOwnPropertyDescriptor`.

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(method, structural)]
      fn bar(this: &Foo);
      #[wasm_bindgen(method, getter, structural)]
      fn baz(this: &Foo) -> u32;
  }
  ```

  The type here, `Foo`, is not required to exist in JS (it's not referenced).
  Instead wasm-bindgen will generate shims that will access the passed in JS
  value's `bar` property to or the `baz` property (depending on the function).

* `js_name = foo` - this can be used to bind to a different function in JS than
  the identifier that's defined in Rust. For example you can also define
  multiple signatures for a polymorphic function in JS as well:

  ```rust
  #[wasm_bindgen]
  extern {
      type Foo;
      #[wasm_bindgen(js_namespace = console, js_name = log)]
      fn log_string(s: &str);
      #[wasm_bindgen(js_namespace = console, js_name = log)]
      fn log_u32(n: u32);
      #[wasm_bindgen(js_namespace = console, js_name = log)]
      fn log_many(a: u32, b: JsValue);
  }
  ```

  All of these functions will call `console.log` in Rust, but each identifier
  will have only one signature in Rust.

* `readonly` - when attached to a `pub` struct field this indicates that it's
  readonly from JS and a setter will not be generated.

  ```rust
  #[wasm_bindgen]
  pub struct Foo {
      pub first: u32,
      #[wasm_bindgen(readonly)]
      pub second: u32,
  }
  ```

  Here the `first` field will be both readable and writable from JS, but the
  `second` field will be a `readonly` field in JS where the setter isn't
  implemented and attempting to set it will throw an exception.


## Rust Type conversions

Previously we've been seeing mostly abridged versions of type conversions when
values enter Rust. Here we'll go into some more depth about how this is
implemented. There are two categories of traits for converting values, traits
for converting values from Rust to JS and traits for the other way around.

### From Rust to JS

First up let's take a look at going from Rust to JS:

```rust
pub trait IntoWasmAbi: WasmDescribe {
    type Abi: WasmAbi;
    fn into_abi(self, extra: &mut Stack) -> Self::Abi;
}
```

And that's it! This is actually the only trait needed currently for translating
a Rust value to a JS one. There's a few points here:

* We'll get to `WasmDescribe` later in this section
* The associated type `Abi` is what will actually be generated as an argument to
  the wasm export. The bound `WasmAbi` is only implemented for types like `u32`
  and `f64`, those which can be placed on the boundary and transmitted
  losslessly.
* And finally we have the `into_abi` function, returning the `Abi` associated
  type which will be actually passed to JS. There's also this `Stack` parameter,
  however. Not all Rust values can be communicated in 32 bits to the `Stack`
  parameter allows transmitting more data, explained in a moment.

This trait is implemented for all types that can be converted to JS and is
unconditionally used during codegen. For example you'll often see `IntoWasmAbi
for Foo` but also `IntoWasmAbi for &'a Foo`.

The `IntoWasmAbi` trait is used in two locations. First it's used to convert
return values of Rust exported functions to JS. Second it's used to convert the
Rust arguments of JS functions imported to Rust.

### From JS to Rust

Unfortunately the opposite direction from above, going from JS to Rust, is a bit
mroe complicated. Here we've got three traits:

```rust
pub trait FromWasmAbi: WasmDescribe {
    type Abi: WasmAbi;
    unsafe fn from_abi(js: Self::Abi, extra: &mut Stack) -> Self;
}

pub trait RefFromWasmAbi: WasmDescribe {
    type Abi: WasmAbi;
    type Anchor: Deref<Target=Self>;
    unsafe fn ref_from_abi(js: Self::Abi, extra: &mut Stack) -> Self::Anchor;
}

pub trait RefMutFromWasmAbi: WasmDescribe {
    type Abi: WasmAbi;
    type Anchor: DerefMut<Target=Self>;
    unsafe fn ref_mut_from_abi(js: Self::Abi, extra: &mut Stack) -> Self::Anchor;
}
```

The `FromWasmAbi` is relatively straightforward, basically the opposite of
`IntoWasmAbi`. It takes the ABI argument (typically the same as
`IntoWasmAbi::Abi`) and then the auxiliary stack to produce an instance of
`Self`. This trait is implemented primarily for types that *don't* have internal
lifetimes or are references.

The latter two traits here are mostly the same, and are intended for generating
references (both shared and mutable references). They look almost the same as
`FromWasmAbi` except that they return an `Anchor` type which implements a
`Deref` trait rather than `Self`.

The `Ref*` traits allow having arguments in functions that are references rather
than bare types, for example `&str`, `&JsValue`, or `&[u8]`. The `Anchor` here
is required to ensure that the lifetimes don't persist beyond one function call
and remain anonymous.

The `From*` family of traits are used for converting the Rust arguments in Rust
exported functions to JS. They are also used for the return value in JS
functions imported into Rust.

### Global stack

Mentioned above not all Rust types will fit within 32 bits. While we can
communicate an `f64` we don't necessarily have the ability to use all the bits.
Types like `&str` need to communicate two items, a pointer and a length (64
bits). Other types like `&Closure<Fn()>` have even more information to
transmit.

As a result we need a method of communicating more data through the signatures
of functions. While we could add more arguments this is somewhat difficult to do
in the world of closures where code generation isn't quite as dynamic as a
procedural macro. Consequently a "global stack" is used to transmit extra
data for a function call.

The global stack is a fixed-sized static allocation in the wasm module. This
stack is temporary scratch space for any one function call from either JS to
Rust or Rust ot JS. Both Rust and the JS shim generated have pointers to this
global stack and will read/write information from it.

Using this scheme whenever we want to pass `&str` from JS to Rust we can pass
the pointer as the actual ABI argument and the length is then placed in the next
spot on the global stack.

The `Stack` argument to the conversion traits above looks like:

```rust
pub trait Stack {
    fn push(&mut self, bits: u32);
    fn pop(&mut self) -> u32;
}
```

A trait is used here to facilitate testing but typically the calls don't end up
being virtually dispatched at runtime.

### Communicating types to `wasm-bindgen`

The last aspect to talk about when converting Rust/JS types amongst one another
is how this information is actually communicated. The `#[wasm_bindgen]` macro is
running over the syntactical (unresolved) structure of the Rust code and is then
responsible for generating information that `wasm-bindgen` the CLI tool later
reads.

To accomplish this a slightly unconventional approach is taken. Static
information about the structure of the Rust code is serialized via JSON
(currently) to a custom section of the wasm executable. Other information, like
what the types actually are, unfortunately isn't known until later in the
compiler due to things like associated type projections and typedefs. It also
turns out that we want to convey "rich" types like `FnMut(String, Foo,
&JsValue)` to the `wasm-bindgen` CLI, and handling all this is pretty tricky!

To solve this issue the `#[wasm_bindgen]` macro generates **executable
functions** which "describe the type signature of an import or export". These
executable functions are what the `WasmDescribe` trait is all about:

```rust
pub trait WasmDescribe {
    fn describe();
}
```

While deceptively simple this trait is actually quite important. When you write,
an export like this:

```rust
#[wasm_bindgen]
fn greet(a: &str) {
    // ...
}
```

In addition to the shims we talked about above which JS generates the macro
*also* generates something like:

```
#[no_mangle]
pub extern fn __wbindgen_describe_greet() {
    <Fn(&str)>::describe();
}
```

Or in other words it generates invocations of `describe` functions. In doing so
the `__wbindgen_describe_greet` shim is a programmatic description of the type
layouts of an import/export. These are then executed when `wasm-bindgen` runs!
These executions rely on an import called `__wbindgen_describe` which passes one
`u32` to the host, and when called multiple times gives a `Vec<u32>`
effectively. This `Vec<u32>` can then be reparsed into an `enum Descriptor`
which fully describes a type.

All in all this is a bit roundabout but shouldn't have any impact on the
generated code or runtime at all. All these descriptor functions are pruned from
the emitted wasm file.

## Wrapping up

That's currently at least what `wasm-bindgen` has to offer! If you've got more
questions though please don't hesitate to ask or open an issue!
