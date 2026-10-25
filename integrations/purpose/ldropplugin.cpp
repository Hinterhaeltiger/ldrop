// SPDX-License-Identifier: GPL-3.0-or-later
//
// The job opens ldrop's share sheet with the shared files and is done: picking the device,
// the PIN and the progress all happen in that window.

#include <KPluginFactory>
#include <QJsonArray>
#include <QProcess>
#include <QUrl>
#include <purpose/pluginbase.h>

namespace
{
class LdropJob : public Purpose::Job
{
    Q_OBJECT
public:
    using Purpose::Job::Job;

    void start() override
    {
        QStringList args{QStringLiteral("--share")};
        const QJsonArray urls = data().value(QLatin1String("urls")).toArray();
        for (const QJsonValue &url : urls) {
            const QUrl parsed(url.toString());
            if (!parsed.isLocalFile()) {
                setError(1);
                setErrorText(QStringLiteral("ldrop can only send local files"));
                emitResult();
                return;
            }
            args << parsed.toLocalFile();
        }
        if (!QProcess::startDetached(QStringLiteral("ldrop-gui"), args)) {
            setError(1);
            setErrorText(QStringLiteral("Cannot start ldrop-gui"));
        }
        emitResult();
    }
};
}

class LdropPlugin : public Purpose::PluginBase
{
    Q_OBJECT
public:
    using Purpose::PluginBase::PluginBase;

    Purpose::Job *createJob() const override
    {
        return new LdropJob(nullptr);
    }
};

K_PLUGIN_CLASS_WITH_JSON(LdropPlugin, "ldropplugin.json")

#include "ldropplugin.moc"
