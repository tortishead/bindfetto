/*
 * Bindfetto DLT Viewer decoder plugin.
 *
 * A thin C++/Qt adapter over the Rust decode core (decode/, via its C ABI): for each
 * DLT message that carries a bindfetto line, it rewrites the raw transaction codes to
 * AIDL method names against a precompiled catalog. All decode logic lives in the core;
 * this file only bridges QDltMsg <-> the C ABI.
 */
#ifndef BINDFETTODECODERPLUGIN_H
#define BINDFETTODECODERPLUGIN_H

#include <QObject>
#include <QString>
#include <QStringList>

#include "plugininterface.h"   // dlt-viewer qdlt SDK
#include "bindfetto_decode.h"  // bindfetto decode core C ABI

#define BINDFETTO_PLUGIN_VERSION "0.1.0"

class BindfettoDecoderPlugin : public QObject,
                               public QDLTPluginInterface,
                               public QDLTPluginDecoderInterface
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.genivi.DLT.Plugin.DLTPluginInterface/1.0"
                      FILE "bindfettodecoderplugin.json")
    Q_INTERFACES(QDLTPluginInterface QDLTPluginDecoderInterface)

public:
    BindfettoDecoderPlugin();
    ~BindfettoDecoderPlugin() override;

    // QDLTPluginInterface
    QString name() override;
    QString description() override;
    QString pluginVersion() override;
    QString pluginInterfaceVersion() override;
    QString error() override;
    bool loadConfig(QString filename) override;
    bool saveConfig(QString filename) override;
    QStringList infoConfig() override;

    // QDLTPluginDecoderInterface
    bool isMsg(QDltMsg &msg, int triggeredByUser) override;
    bool decodeMsg(QDltMsg &msg, int triggeredByUser) override;

private:
    // Merge every *.json under `dir` (recursively) into one catalog JSON in `out`.
    bool mergeCatalogDir(const QString &dir, QByteArray &out);

    BfDecoder *m_decoder = nullptr;  // owned; NULL until a catalog is loaded
    QString m_catalogPath;
    QString m_error;
};

#endif // BINDFETTODECODERPLUGIN_H
