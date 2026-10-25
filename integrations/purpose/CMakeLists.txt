# SPDX-License-Identifier: GPL-3.0-or-later
# ldrop in the KDE Share menu (Dolphin, Gwenview, Spectacle, Okular, …): a Purpose plugin
# that hands the files to `ldrop-gui --share`.
cmake_minimum_required(VERSION 3.22)
project(ldrop-purpose LANGUAGES CXX)

# extra-cmake-modules supplies KDE's install layout; without it, use Qt's plugin directory.
find_package(ECM 6.0 QUIET NO_MODULE)
if(ECM_FOUND)
    set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})
    include(KDEInstallDirs)
    include(KDECMakeSettings)
    include(KDECompilerSettings NO_POLICY_SCOPE)
else()
    set(CMAKE_AUTOMOC ON)
    set(CMAKE_CXX_STANDARD 20)
    set(BUILD_SHARED_LIBS ON)
    set(CMAKE_LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin")
    set(KDE_INSTALL_PLUGINDIR "lib/qt6/plugins" CACHE PATH "Qt plugin directory, relative to the prefix")
endif()

find_package(Qt6 6.5 REQUIRED COMPONENTS Core)
find_package(KF6CoreAddons 6.0 REQUIRED)
find_package(KF6Purpose 6.0 REQUIRED)

kcoreaddons_add_plugin(ldropplugin SOURCES ldropplugin.cpp INSTALL_NAMESPACE "kf6/purpose")
target_link_libraries(ldropplugin Qt6::Core KF6::CoreAddons KF6::Purpose)
set_target_properties(ldropplugin PROPERTIES PREFIX "")
