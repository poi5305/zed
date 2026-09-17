/* Completes tree-sitter-language's wasm libc headers (0.1.8 / git 43623ec) for
 * grammar scanners that assume a hosted <ctype.h>/<assert.h>/<wctype.h>.
 *
 * - isdigit: wasm <ctype.h> only declares isblank/isprint. bash 0.25.1 and
 *   tree-sitter-md call isdigit; latest crates.io bash is 0.25.1 so this
 *   cannot be a version bump.
 * - wchar_t: wasm <wctype.h> does not define it (host wctype.h usually pulls
 *   it in via <wchar.h>). tree-sitter-cpp 0.23.4's scanner uses wchar_t
 *   without including <wchar.h>; the desktop pin is a git rev, not a crates.io
 *   float.
 * - static_assert: wasm <assert.h> only defines assert(). C11 scanners
 *   (tree-sitter-cpp) expect the assert.h macro.
 *
 * Forced-included by web/build.sh via -include. */
#ifndef ZED_WEB_GRAMMAR_WASM_COMPAT_H
#define ZED_WEB_GRAMMAR_WASM_COMPAT_H

#include <wchar.h>

#ifndef static_assert
#define static_assert _Static_assert
#endif

static inline int isdigit(int c) { return (unsigned)(c - '0') <= 9u; }

#endif
