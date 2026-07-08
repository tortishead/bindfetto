cmake_minimum_required(VERSION 3.16)
project(bindfetto_dlt_plugin LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

# A DLT Viewer decoder plugin needs only QtCore (QString/QStringList/QObject).
find_package(QT NAMES Qt6 Qt5 REQUIRED COMPONENTS Core)
find_package(Qt${QT_VERSION_MAJOR} REQUIRED COMPONENTS Core)

# --- dlt-viewer qdlt SDK (headers + library) -------------------------------------
# Point these at your dlt-viewer checkout/build. The headers are the qdlt export dir
# (plugininterface.h, qdltmsg.h, qdltargument.h, qdlt.h); the library is libqdlt.
set(DLT_VIEWER_QDLT_INCLUDE_DIR "" CACHE PATH
    "dlt-viewer qdlt export headers (plugininterface.h, qdltmsg.h, ...)")
find_library(DLT_VIEWER_QDLT_LIB NAMES qdlt
    DOC "dlt-viewer qdlt shared library (libqdlt)")

# --- bindfetto Rust decode core (C ABI) ------------------------------------------
set(BINDFETTO_DECODE_INCLUDE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../decode/include"
    CACHE PATH "location of bindfetto_decode.h")
# Prefer the *static* archive so the core is embedded in the plugin (a self-contained
# .so with no dev-path dylib dependency). Restrict the suffix search to .a.
set(_bf_saved_suffixes ${CMAKE_FIND_LIBRARY_SUFFIXES})
set(CMAKE_FIND_LIBRARY_SUFFIXES .a)
find_library(BINDFETTO_DECODE_LIB NAMES bindfetto_decode
    PATHS "${CMAKE_CURRENT_SOURCE_DIR}/../../decode/target/release"
    NO_DEFAULT_PATH
    DOC "Rust decode core static library (build with: cargo build --release in decode/)")
set(CMAKE_FIND_LIBRARY_SUFFIXES ${_bf_saved_suffixes})

if(NOT DLT_VIEWER_QDLT_INCLUDE_DIR OR NOT DLT_VIEWER_QDLT_LIB)
    message(FATAL_ERROR
        "Set DLT_VIEWER_QDLT_INCLUDE_DIR and DLT_VIEWER_QDLT_LIB to your dlt-viewer SDK. See README.md")
endif()
if(NOT BINDFETTO_DECODE_LIB)
    message(FATAL_ERROR
        "libbindfetto_decode not found. Build it: (cd decode && cargo build --release)")
endif()

add_library(bindfettodecoderplugin MODULE
    bindfettodecoderplugin.cpp
    bindfettodecoderplugin.h
)

target_include_directories(bindfettodecoderplugin PRIVATE
    ${DLT_VIEWER_QDLT_INCLUDE_DIR}
    ${BINDFETTO_DECODE_INCLUDE_DIR}
)

target_link_libraries(bindfettodecoderplugin PRIVATE
    Qt${QT_VERSION_MAJOR}::Core
    ${DLT_VIEWER_QDLT_LIB}
    ${BINDFETTO_DECODE_LIB}
)

# The Rust static library pulls in platform runtime deps at final link.
if(APPLE)
    target_link_libraries(bindfettodecoderplugin PRIVATE
        "-framework CoreFoundation" "-framework Security")
elseif(UNIX)
    target_link_libraries(bindfettodecoderplugin PRIVATE pthread dl m)
endif()
