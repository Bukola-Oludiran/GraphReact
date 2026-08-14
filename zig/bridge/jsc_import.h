/*
 * Zig 0.16 translates Mach descriptor structs containing packed bitfields as
 * opaque types. Xcode 26's arm64 SDK then applies sizeof-based XNU assertions
 * to those opaque translations, even though the JavaScriptCore bridge never
 * references the Mach descriptors. Include mach/port.h first so its helper
 * macros can be replaced with no-ops before JavaScriptCore pulls message.h in.
 * The declarations and target architecture remain otherwise unchanged.
 */
#include <mach/port.h>

#undef xnu_static_assert_struct_size
#undef xnu_static_assert_struct_size_kernel_user
#undef xnu_static_assert_struct_size_kernel_user64_user32

#define xnu_static_assert_struct_size(name, expected_size)
#define xnu_static_assert_struct_size_kernel_user(name, expected_kernel_size, expected_user_size)
#define xnu_static_assert_struct_size_kernel_user64_user32(name, expected_kernel_size, expected_user64_size, expected_user32_size)

#include <JavaScriptCore/JavaScriptCore.h>
