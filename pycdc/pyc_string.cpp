#include "pyc_string.h"
#include "pyc_module.h"
#include "data.h"
#include <stdexcept>

static bool check_ascii(const std::string& data)
{
    for (char ch : data) {
        if (static_cast<unsigned char>(ch) & 0x80)
            return false;
    }
    return true;
}

// 判断 data 整体是否为合法 UTF-8 序列（拒绝超长编码、surrogate 和 > U+10FFFF）
static bool check_utf8(const std::string& data)
{
    const unsigned char* p = reinterpret_cast<const unsigned char*>(data.data());
    const unsigned char* end = p + data.size();

    while (p != end) {
        unsigned char lead = *p;
        size_t len;
        unsigned char contLo, contHi; // 允许的第二个字节区间

        if (lead < 0x80) {            // ASCII
            ++p;
            continue;
        } else if (lead >= 0xC2 && lead <= 0xDF) {
            len = 2; contLo = 0x80; contHi = 0xBF;
        } else if (lead == 0xE0) {    // 排除超长编码
            len = 3; contLo = 0xA0; contHi = 0xBF;
        } else if (lead >= 0xE1 && lead <= 0xEC) {
            len = 3; contLo = 0x80; contHi = 0xBF;
        } else if (lead == 0xED) {    // 排除 UTF-16 surrogate
            len = 3; contLo = 0x80; contHi = 0x9F;
        } else if (lead >= 0xEE && lead <= 0xEF) {
            len = 3; contLo = 0x80; contHi = 0xBF;
        } else if (lead == 0xF0) {    // 排除超长编码
            len = 4; contLo = 0x90; contHi = 0xBF;
        } else if (lead >= 0xF1 && lead <= 0xF3) {
            len = 4; contLo = 0x80; contHi = 0xBF;
        } else if (lead == 0xF4) {    // 排除 > U+10FFFF
            len = 4; contLo = 0x80; contHi = 0x8F;
        } else {
            return false;             // 0x80-0xC1 或 0xF5-0xFF：非法的首字节
        }

        if (static_cast<size_t>(end - p) < len)
            return false;
        if (p[1] < contLo || p[1] > contHi)
            return false;
        for (size_t k = 2; k < len; ++k) {
            if ((p[k] & 0xC0) != 0x80)
                return false;
        }
        p += len;
    }
    return true;
}

/* PycString */
void PycString::load(PycData* stream, PycModule* mod)
{
    if (type() == TYPE_STRINGREF) {
        PycRef<PycString> str = mod->getIntern(stream->get32());
        m_type = str->m_type;
        m_value = str->m_value;
    } else {
        int length;
        if (type() == TYPE_SHORT_ASCII || type() == TYPE_SHORT_ASCII_INTERNED)
            length = stream->getByte();
        else
            length = stream->get32();

        if (length < 0)
            throw std::bad_alloc();

        m_value.resize(length);
        if (length) {
            stream->getBuffer(length, &m_value.front());
            if (type() == TYPE_ASCII || type() == TYPE_ASCII_INTERNED ||
                    type() == TYPE_SHORT_ASCII || type() == TYPE_SHORT_ASCII_INTERNED) {
                if (!check_ascii(m_value))
                    throw std::runtime_error("Invalid bytes in ASCII string");
            }
        }

        if (type() == TYPE_INTERNED || type() == TYPE_ASCII_INTERNED ||
                type() == TYPE_SHORT_ASCII_INTERNED)
            mod->intern(this);
    }
}

bool PycString::isEqual(PycRef<PycObject> obj) const
{
    if (type() != obj.type())
        return false;

    PycRef<PycString> strObj = obj.cast<PycString>();
    return isEqual(strObj->m_value);
}

void PycString::print(std::ostream &pyc_output, PycModule* mod, bool triple,
                      const char* parent_f_string_quote)
{
    char prefix = 0;
    switch (type()) {
    case TYPE_STRING:
        prefix = mod->strIsUnicode() ? 'b' : 0;
        break;
    case PycObject::TYPE_UNICODE:
        prefix = mod->strIsUnicode() ? 0 : 'u';
        break;
    case PycObject::TYPE_INTERNED:
        prefix = mod->internIsBytes() ? 'b' : 0;
        break;
    case PycObject::TYPE_ASCII:
    case PycObject::TYPE_ASCII_INTERNED:
    case PycObject::TYPE_SHORT_ASCII:
    case PycObject::TYPE_SHORT_ASCII_INTERNED:
        // These types don't exist until Python 3.4
        prefix = 0;
        break;
    default:
        throw std::runtime_error("Invalid string type");
    }

    if (prefix != 0)
        pyc_output << prefix;

    if (m_value.empty()) {
        pyc_output << "''";
        return;
    }

    // 若原始字节整体是合法 UTF-8，就直接写出，避免把可读的中文输出成
    // \xe7\xae\x80 的形式。
    //   - TYPE_UNICODE：pyc 里的 unicode 本就按 UTF-8 保存，交给流解释即可；
    //   - Python 2 的 str / interned 是 bytes，当且仅当字节流合法时才原样输出，
    //     否则仍走 \xNN —— 这样输出文件始终是合法 UTF-8；
    //   - Python 3 的 bytes（strIsUnicode() 为 true）始终走 \xNN，
    //     因为 py3 源码里的 bytes 字面量不能直接包含非 ASCII 字符。
    bool rawBytes = (type() == TYPE_UNICODE);
    if (!rawBytes && !mod->strIsUnicode() &&
            (type() == TYPE_STRING || (type() == TYPE_INTERNED && !mod->internIsBytes()))) {
        rawBytes = check_utf8(m_value);
    }

    // Determine preferred quote style (Emulate Python's method)
    bool useQuotes = false;
    if (!parent_f_string_quote) {
        for (char ch : m_value) {
            if (ch == '\'') {
                useQuotes = true;
            } else if (ch == '"') {
                useQuotes = false;
                break;
            }
        }
    } else {
        useQuotes = parent_f_string_quote[0] == '"';
    }

    // Output the string
    if (!parent_f_string_quote) {
        if (triple)
            pyc_output << (useQuotes ? R"(""")" : "'''");
        else
            pyc_output << (useQuotes ? '"' : '\'');
    }
    for (char ch : m_value) {
        if (static_cast<unsigned char>(ch) < 0x20 || ch == 0x7F) {
            if (ch == '\r') {
                pyc_output << "\\r";
            } else if (ch == '\n') {
                if (triple)
                    pyc_output << '\n';
                else
                    pyc_output << "\\n";
            } else if (ch == '\t') {
                pyc_output << "\\t";
            } else {
                formatted_print(pyc_output, "\\x%02x", (ch & 0xFF));
            }
        } else if (static_cast<unsigned char>(ch) >= 0x80) {
            if (rawBytes) {
                // 字节流已是合法 UTF-8，让流原样写出
                pyc_output << ch;
            } else {
                formatted_print(pyc_output, "\\x%02x", (ch & 0xFF));
            }
        } else {
            if (!useQuotes && ch == '\'')
                pyc_output << R"(\')";
            else if (useQuotes && ch == '"')
                pyc_output << R"(\")";
            else if (ch == '\\')
                pyc_output << R"(\\)";
            else if (parent_f_string_quote && ch == '{')
                pyc_output << "{{";
            else if (parent_f_string_quote && ch == '}')
                pyc_output << "}}";
            else
                pyc_output << ch;
        }
    }
    if (!parent_f_string_quote) {
        if (triple)
            pyc_output << (useQuotes ? R"(""")" : "'''");
        else
            pyc_output << (useQuotes ? '"' : '\'');
    }
}
