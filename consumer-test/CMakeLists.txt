cmake_minimum_required(VERSION 3.20)
project(gentlite_consumer_test LANGUAGES CXX)

# GenICam SDK (GenApi reference implementation). Headers under
# library/CPP/include, GenTL.h/PFNC.h at the root, VC141 MD import libs under
# library/CPP/lib/Win64_x64, runtime DLLs under bin/Win64_x64.
# No default location: point cmake at your SDK, e.g.
#   cmake -B build -DGENAPI_ROOT="C:/Program Files/GenICam"
set(GENAPI_ROOT "" CACHE PATH
    "Root of the EMVA GenICam SDK (contains library/CPP/include)")

if(NOT GENAPI_ROOT)
    message(FATAL_ERROR
        "GENAPI_ROOT is not set. This project needs the EMVA GenICam SDK\n"
        "(the GenApi reference implementation, e.g. GenICam_Package). "
        "Configure with:\n"
        "  cmake -B build -DGENAPI_ROOT=<path to the GenICam SDK>")
endif()

find_path(GENAPI_INCLUDE_DIR
    NAMES GenApi/GenApi.h
    PATHS "${GENAPI_ROOT}/library/CPP/include"
    NO_DEFAULT_PATH)
find_file(GENAPI_GENTL_HEADER
    NAMES GenTL.h
    PATHS "${GENAPI_ROOT}"
    NO_DEFAULT_PATH)
find_library(GENAPI_LIBRARY
    NAMES GenApi_MD_VC141_v3_5
    PATHS "${GENAPI_ROOT}/library/CPP/lib/Win64_x64"
    NO_DEFAULT_PATH)
find_library(GCBASE_LIBRARY
    NAMES GCBase_MD_VC141_v3_5
    PATHS "${GENAPI_ROOT}/library/CPP/lib/Win64_x64"
    NO_DEFAULT_PATH)

if(NOT GENAPI_INCLUDE_DIR OR NOT GENAPI_GENTL_HEADER OR NOT GENAPI_LIBRARY OR NOT GCBASE_LIBRARY)
    message(FATAL_ERROR
        "GenICam SDK incomplete under GENAPI_ROOT=${GENAPI_ROOT}\n"
        "  include dir : ${GENAPI_INCLUDE_DIR}\n"
        "  GenTL.h     : ${GENAPI_GENTL_HEADER}\n"
        "  GenApi lib  : ${GENAPI_LIBRARY}\n"
        "  GCBase lib  : ${GCBASE_LIBRARY}\n"
        "Set -DGENAPI_ROOT=<path to GenICam SDK>.")
endif()

message(STATUS "GenICam SDK: ${GENAPI_ROOT}")

add_executable(test_genapi test_genapi.cpp)
target_include_directories(test_genapi PRIVATE "${GENAPI_INCLUDE_DIR}" "${GENAPI_ROOT}")
target_link_libraries(test_genapi PRIVATE "${GENAPI_LIBRARY}" "${GCBASE_LIBRARY}")
target_compile_features(test_genapi PRIVATE cxx_std_17)
set_target_properties(test_genapi PROPERTIES
    CXX_EXTENSIONS OFF
    # The GenICam reference DLLs are built with the dynamic CRT (/MD); a
    # static-CRT consumer would keep a separate heap and break cross-DLL
    # string/free handling.
    MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")
if(MSVC)
    target_compile_definitions(test_genapi PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN _CRT_SECURE_NO_WARNINGS)
    # Sources are UTF-8 regardless of the system code page.
    target_compile_options(test_genapi PRIVATE /utf-8)
endif()
