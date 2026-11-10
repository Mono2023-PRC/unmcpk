#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>
#include "ASTree.h"

#ifdef WIN32
#  define PATHSEP '\\'
#  include <direct.h>
#  include <windows.h>
#else
#  define PATHSEP '/'
#  include <dirent.h>
#  include <sys/stat.h>
#endif

static bool statPath(const std::string& path, bool* isDirectory)
{
    if (path.empty())
        return false;
#ifdef WIN32
    struct _stat st;
    if (_stat(path.c_str(), &st) != 0)
        return false;
#else
    struct stat st;
    if (stat(path.c_str(), &st) != 0)
        return false;
#endif
    if (isDirectory != nullptr)
        *isDirectory = (st.st_mode & S_IFDIR) != 0;
    return true;
}

static bool isDir(const std::string& path)
{
    bool directory = false;
    return statPath(path, &directory) && directory;
}

static bool fileExists(const std::string& path)
{
    bool directory = false;
    return statPath(path, &directory) && !directory;
}

static void mkdirOne(const std::string& path)
{
#ifdef WIN32
    _mkdir(path.c_str());
#else
    mkdir(path.c_str(), 0755);
#endif
}

// mkdir -p, up to and including the last component of path
static bool makeDirs(const std::string& path)
{
    if (path.empty())
        return false;

    std::string cur;
    size_t i = 0;
#ifdef WIN32
    if (path.size() >= 2 && path[1] == ':') { // Keep the drive letter together
        cur = path.substr(0, 2);
        i = 2;
    }
#endif
    while (i < path.size()) {
        if (path[i] == '/' || path[i] == '\\') {
            if (!cur.empty() && cur[cur.size() - 1] != ':' && !isDir(cur))
                mkdirOne(cur);
        }
        cur.push_back(path[i]);
        ++i;
    }
    if (!cur.empty() && !isDir(cur))
        mkdirOne(cur);
    return isDir(path);
}

static std::string joinPath(const std::string& dir, const std::string& name)
{
    if (dir.empty())
        return name;
    if (name.empty())
        return dir;
    char last = dir[dir.size() - 1];
    if (last == '/' || last == '\\')
        return dir + name;
    return dir + PATHSEP + name;
}

static std::string baseName(const std::string& path)
{
    size_t pos = path.find_last_of("/\\");
    return (pos == std::string::npos) ? path : path.substr(pos + 1);
}

static std::string dirName(const std::string& path)
{
    size_t pos = path.find_last_of("/\\");
    return (pos == std::string::npos) ? std::string() : path.substr(0, pos);
}

// Turns the source path recorded in a module into something that can safely be
// appended to the output directory: no drive letter, no absolute prefix and no
// '.' or '..' components.
static std::string sanitizeRelative(const std::string& name)
{
    std::string normalized = name;
    for (size_t i = 0; i < normalized.size(); ++i) {
        if (normalized[i] == '\\')
            normalized[i] = '/';
    }
#ifdef WIN32
    if (normalized.size() >= 2 && normalized[1] == ':')
        normalized = normalized.substr(2);
#endif

    std::string out;
    size_t start = 0;
    while (start < normalized.size()) {
        size_t end = normalized.find('/', start);
        if (end == std::string::npos)
            end = normalized.size();
        std::string part = normalized.substr(start, end - start);
        start = end + 1;

        if (part.empty() || part == "." || part == "..")
            continue;
        if (!out.empty())
            out += PATHSEP;
        out += part;
    }
    return out;
}

static void collectPyc(const std::string& dir, std::vector<std::string>& out)
{
#ifdef WIN32
    WIN32_FIND_DATAA entry;
    HANDLE handle = FindFirstFileA(joinPath(dir, "*").c_str(), &entry);
    if (handle == INVALID_HANDLE_VALUE)
        return;
    do {
        std::string name = entry.cFileName;
        if (name == "." || name == "..")
            continue;
        std::string full = joinPath(dir, name);
        if ((entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0) {
            collectPyc(full, out);
        } else if (name.size() > 4 && _stricmp(name.c_str() + name.size() - 4, ".pyc") == 0) {
            out.push_back(full);
        }
    } while (FindNextFileA(handle, &entry));
    FindClose(handle);
#else
    DIR* handle = opendir(dir.c_str());
    if (handle == nullptr)
        return;
    while (struct dirent* entry = readdir(handle)) {
        std::string name = entry->d_name;
        if (name == "." || name == "..")
            continue;
        std::string full = joinPath(dir, name);
        if (isDir(full))
            collectPyc(full, out);
        else if (name.size() > 4 && name.compare(name.size() - 4, 4, ".pyc") == 0)
            out.push_back(full);
    }
    closedir(handle);
#endif
}

static std::string moduleFileName(PycModule& mod)
{
    PycRef<PycCode> code = mod.code();
    if (code == NULL)
        return std::string();

    PycRef<PycString> name = code->fileName();
    if (name == NULL)
        return std::string();

    return name->strValue();
}

static void writeHeader(std::ostream& out, const std::string& dispname,
                        const std::string& embeddedName, PycModule& mod)
{
    // Python 2 source is decoded as ASCII unless a PEP 263 declaration is
    // present, so it has to be emitted for Python 2 targets.
    if (mod.majorVer() < 3)
        out << "# -*- coding: utf-8 -*-\n";
    out << "# Source Generated with Decompyle++\n";
    formatted_print(out, "# File: %s (Python %d.%d%s)\n", dispname.c_str(),
                    mod.majorVer(), mod.minorVer(),
                    (mod.majorVer() < 3 && mod.isUnicode()) ? " Unicode" : "");
    if (!embeddedName.empty())
        formatted_print(out, "# Embedded file name: %s\n", embeddedName.c_str());
    out << "\n";
}

static bool writeOutputFile(const std::string& path, const std::string& data)
{
    std::string parent = dirName(path);
    if (!parent.empty() && !isDir(parent) && !makeDirs(parent)) {
        fprintf(stderr, "Error creating directory for '%s'\n", path.c_str());
        return false;
    }

    std::ofstream out(path, std::ios_base::out | std::ios_base::binary);
    if (out.fail()) {
        fprintf(stderr, "Error opening file '%s' for writing\n", path.c_str());
        return false;
    }
    out << data;
    return true;
}

static void reportProgress(bool enabled, size_t index, size_t total,
                           const std::string& path, bool ok)
{
    if (!enabled)
        return;
    fprintf(stderr, "[%zu/%zu] %s %s\n", index + 1, total,
            ok ? "->" : "!!", path.c_str());
    fflush(stderr);
}

static void showUsage(const char* argv0)
{
    fprintf(stderr, "Usage:  %s [options] input.pyc|input-dir ...\n\n", argv0);
    fputs("Options:\n", stderr);
    fputs("  -o <path>       Write output to <path>: either a file, or a directory in\n", stderr);
    fputs("                  which every module is stored under the source path\n", stderr);
    fputs("                  recorded inside it (default: stdout)\n", stderr);
    fputs("  -c              Specify loading a compiled code object. Requires the version to be set\n", stderr);
    fputs("  -v <x.y>        Specify a Python version for loading a compiled code object\n", stderr);
    fputs("  --help          Show this help text and then exit\n", stderr);
}

static bool loadModule(PycModule& mod, const std::string& infile, bool marshalled,
                       int major, int minor)
{
    if (!marshalled) {
        try {
            mod.loadFromFile(infile.c_str());
        } catch (std::exception& ex) {
            fprintf(stderr, "Error loading file %s: %s\n", infile.c_str(), ex.what());
            return false;
        }
    } else {
        mod.loadFromMarshalledFile(infile.c_str(), major, minor);
    }

    if (!mod.isValid()) {
        fprintf(stderr, "Could not load file %s\n", infile.c_str());
        return false;
    }
    return true;
}

int main(int argc, char* argv[])
{
    std::string outPath;
    bool haveOut = false;
    bool marshalled = false;
    int major = -1, minor = -1;
    std::vector<std::string> inputs;

    for (int arg = 1; arg < argc; ++arg) {
        if (strcmp(argv[arg], "-o") == 0) {
            if (arg + 1 < argc) {
                outPath = argv[++arg];
                haveOut = true;
            } else {
                fputs("Option '-o' requires a filename\n", stderr);
                return 1;
            }
        } else if (strcmp(argv[arg], "-c") == 0) {
            marshalled = true;
        } else if (strcmp(argv[arg], "-v") == 0) {
            if (arg + 1 < argc) {
                std::string version = argv[++arg];
                auto dot = version.find('.');
                if (dot == std::string::npos || dot == version.size() - 1) {
                    fputs("Unable to parse version string (use the format x.y)\n", stderr);
                    return 1;
                }
                major = std::stoi(version.substr(0, dot));
                minor = std::stoi(version.substr(dot + 1, version.size()));
            } else {
                fputs("Option '-v' requires a version\n", stderr);
                return 1;
            }
        } else if (strcmp(argv[arg], "--help") == 0 || strcmp(argv[arg], "-h") == 0) {
            showUsage(argv[0]);
            return 0;
        } else {
            inputs.push_back(argv[arg]);
        }
    }

    if (inputs.empty()) {
        showUsage(argv[0]);
        return 1;
    }

    if (marshalled && (major < 0 || minor < 0)) {
        fputs("Opening raw code objects requires a version to be specified\n", stderr);
        return 1;
    }

    // Expand directory arguments into the .pyc files below them.
    std::vector<std::string> files;
    bool haveDirInput = false;
    for (const std::string& input : inputs) {
        if (isDir(input)) {
            haveDirInput = true;
            collectPyc(input, files);
        } else {
            files.push_back(input);
        }
    }

    // '-o' names a directory unless it is (or looks like) a Python source file
    // and there is a single module to write: a whole tree or several modules
    // cannot go into one file.
    bool outIsDir = false;
    if (haveOut) {
        char last = outPath.empty() ? '\0' : outPath[outPath.size() - 1];
        bool looksLikeSource = outPath.size() > 3 &&
                outPath.compare(outPath.size() - 3, 3, ".py") == 0;
        outIsDir = isDir(outPath) || haveDirInput || files.size() > 1 ||
                   last == '/' || last == '\\' || !looksLikeSource;
        if (!outIsDir && !isDir(dirName(outPath)))
            makeDirs(dirName(outPath));
        else if (outIsDir && !isDir(outPath))
            makeDirs(outPath);
    }

    if (files.empty()) {
        fputs("No .pyc files found\n", stderr);
        return 1;
    }

    // Progress goes to stderr so that it cannot end up in decompiled output.
    bool showProgress = haveDirInput || files.size() > 1;

    int failures = 0;
    for (size_t index = 0; index < files.size(); ++index) {
        const std::string& file = files[index];
        PycModule mod;
        if (!loadModule(mod, file, marshalled, major, minor)) {
            ++failures;
            reportProgress(showProgress, index, files.size(), file, false);
            continue;
        }

        std::string embedded = moduleFileName(mod);
        std::ostringstream output;
        writeHeader(output, baseName(file), embedded, mod);
        try {
            decompyle(mod.code(), &mod, output);
        } catch (std::exception& ex) {
            fprintf(stderr, "Error decompyling %s: %s\n", file.c_str(), ex.what());
            ++failures;
            reportProgress(showProgress, index, files.size(), file, false);
            continue;
        }

        if (!haveOut) {
            std::cout << output.str();
            reportProgress(showProgress, index, files.size(), file, true);
            continue;
        }

        std::string target;
        if (outIsDir) {
            std::string name = embedded.empty() ? baseName(file) : embedded;
            target = joinPath(outPath, sanitizeRelative(name));
            // Several modules can claim the same source path (the same file
            // compiled more than once, for instance).  Keep both by putting
            // the name of the .pyc file into the target name.
            if (isDir(target) || fileExists(target)) {
                std::string stem = baseName(file);
                if (stem.size() > 4 && stem.compare(stem.size() - 4, 4, ".pyc") == 0)
                    stem = stem.substr(0, stem.size() - 4);

                std::string renamed = target;
                size_t dot = baseName(target).rfind('.');
                if (dot != std::string::npos && dot != 0)
                    renamed.insert(target.size() - baseName(target).size() + dot, "." + stem);
                else
                    renamed += "." + stem;

                fprintf(stderr, "Warning: %s also maps to %s, writing it as %s\n",
                        file.c_str(), target.c_str(), renamed.c_str());
                target = renamed;
            }
        } else {
            target = outPath;
        }

        bool written = writeOutputFile(target, output.str());
        if (!written)
            ++failures;
        reportProgress(showProgress, index, files.size(),
                       written ? target : file, written);
    }

    return (failures != 0) ? 1 : 0;
}
