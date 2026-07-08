#include "bindfettodecoderplugin.h"

#include <QByteArray>
#include <QFile>

#include "qdltmsg.h"       // QDltMsg
#include "qdltargument.h"  // QDltArgument

// bindfetto lines carry this marker so they're identifiable even after a
// logcat->DLT bridge flattens the `bindfetto` tag into the payload text.
static const QLatin1String kMarker("BINDFETTO");

BindfettoDecoderPlugin::BindfettoDecoderPlugin() = default;

BindfettoDecoderPlugin::~BindfettoDecoderPlugin()
{
    if (m_decoder) {
        bf_decoder_free(m_decoder);
        m_decoder = nullptr;
    }
}

QString BindfettoDecoderPlugin::name()
{
    return QStringLiteral("Bindfetto Decoder Plugin");
}

QString BindfettoDecoderPlugin::description()
{
    return QStringLiteral(
        "Resolves bindfetto Binder transaction codes to AIDL method names using a "
        "precompiled catalog.");
}

QString BindfettoDecoderPlugin::pluginVersion()
{
    return QStringLiteral(BINDFETTO_PLUGIN_VERSION);
}

QString BindfettoDecoderPlugin::pluginInterfaceVersion()
{
    // Must match the dlt-viewer SDK this plugin is built against.
    return QStringLiteral(PLUGIN_INTERFACE_VERSION);
}

QString BindfettoDecoderPlugin::error()
{
    return m_error;
}

// The plugin's "config file" (set in the DLT Viewer plugin manager) is the AIDL
// catalog JSON produced by the Track B1 builder.
bool BindfettoDecoderPlugin::loadConfig(QString filename)
{
    QFile file(filename);
    if (!file.open(QIODevice::ReadOnly | QIODevice::Text)) {
        m_error = QStringLiteral("cannot open catalog: %1").arg(filename);
        return false;
    }
    const QByteArray json = file.readAll();
    file.close();

    BfDecoder *decoder = bf_decoder_new(json.constData());
    if (!decoder) {
        m_error = QStringLiteral("invalid catalog JSON: %1").arg(filename);
        return false;
    }

    if (m_decoder) {
        bf_decoder_free(m_decoder);
    }
    m_decoder = decoder;
    m_catalogPath = filename;
    m_error.clear();
    return true;
}

bool BindfettoDecoderPlugin::saveConfig(QString /*filename*/)
{
    return true;  // catalog is read-only; nothing to persist
}

QStringList BindfettoDecoderPlugin::infoConfig()
{
    QStringList info;
    if (m_decoder) {
        info << QStringLiteral("catalog: %1").arg(m_catalogPath);
    } else {
        info << QStringLiteral("no catalog loaded");
    }
    return info;
}

bool BindfettoDecoderPlugin::isMsg(QDltMsg &msg, int triggeredByUser)
{
    Q_UNUSED(triggeredByUser)
    if (!m_decoder) {
        return false;
    }
    return msg.toStringPayload().contains(kMarker);
}

bool BindfettoDecoderPlugin::decodeMsg(QDltMsg &msg, int triggeredByUser)
{
    Q_UNUSED(triggeredByUser)
    if (!m_decoder) {
        return false;
    }

    const QString payload = msg.toStringPayload();
    const QByteArray in = payload.toUtf8();

    char *decoded_c = bf_decode_line(m_decoder, in.constData());
    if (!decoded_c) {
        return false;
    }
    const QString decoded = QString::fromUtf8(decoded_c);
    bf_string_free(decoded_c);

    // Nothing resolved (no known codes): leave the message untouched rather than
    // flatten its existing argument structure.
    if (decoded == payload) {
        return true;
    }

    // Replace the payload with a single UTF-8 string argument holding the decoded
    // line. toStringPayload() renders a string argument's bytes directly, so this is
    // exactly what the viewer displays.
    QDltArgument arg;
    arg.setTypeInfo(QDltArgument::DltTypeInfoUtf8);
    arg.setEndianness(QDlt::DltEndiannessLittleEndian);
    arg.setOffsetPayload(0);
    arg.setData(decoded.toUtf8());

    msg.clearArguments();
    msg.addArgument(arg);
    return true;
}
