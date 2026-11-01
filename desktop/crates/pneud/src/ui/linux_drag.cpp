#include <QtWidgets/QWidget>
#include <QtGui/QDropEvent>
#include <QtCore/QMimeData>
#include <QtCore/QPointer>
#include <QtCore/QUrl>

using PneuDropCallback = int (*)(void*, int, double, double, const unsigned char*, size_t);
class PneuDropFilter final : public QObject {
    QPointer<QWidget> widget;
    PneuDropCallback callback;
    void* context;
    bool previousAcceptDrops;
public:
    PneuDropFilter(QWidget* w, PneuDropCallback cb, void* ctx)
        : QObject(w), widget(w), callback(cb), context(ctx), previousAcceptDrops(w->acceptDrops()) {
        w->setAcceptDrops(true);
        w->installEventFilter(this);
    }
    ~PneuDropFilter() override {
        if (widget) {
            widget->removeEventFilter(this);
            widget->setAcceptDrops(previousAcceptDrops);
        }
        callback(context, 5, 0, 0, nullptr, 0);
    }
    bool eventFilter(QObject*, QEvent* event) override {
        const auto type = event->type();
        if (type == QEvent::Hide || type == QEvent::Destroy || type == QEvent::DragLeave) {
            callback(context, 4, 0, 0, nullptr, 0);
            if (type != QEvent::DragLeave) return false;
            event->accept();
            return true;
        }
        const int kind = type == QEvent::DragEnter ? 1 : type == QEvent::DragMove ? 2 : type == QEvent::Drop ? 3 : 0;
        if (!kind) return false;
        auto* drop = static_cast<QDropEvent*>(event);
        QByteArray data;
        bool valid = drop->possibleActions().testFlag(Qt::CopyAction);
        if (kind != 2 && valid) {
            const auto urls = drop->mimeData()->urls();
            valid = !urls.empty() && urls.size() <= 128;
            if (valid) for (const auto& url : urls) {
                const auto encoded = url.toEncoded(QUrl::FullyEncoded);
                if (!url.isLocalFile() || encoded.isEmpty() || encoded.size() > 32768) {
                    valid = false;
                    break;
                }
                data += encoded;
                data.append('\0');
            }
        }
        // Qt 5 uses posF(); Qt 6 uses position(). Both are local logical
        // coordinates, already in the units used by Slint's row geometry.
        #if QT_VERSION >= QT_VERSION_CHECK(6, 0, 0)
        const auto point = drop->position();
        #else
        const auto point = drop->posF();
        #endif
        const bool accepted = valid && callback(context, kind, point.x(), point.y(),
            reinterpret_cast<const unsigned char*>(data.constData()), size_t(data.size()));
        if (kind == 3 || (kind == 1 && !accepted)) callback(context, 4, 0, 0, nullptr, 0);
        if (accepted) {
            drop->setDropAction(Qt::CopyAction);
            drop->accept();
        } else drop->ignore();
        return true;
    }
};
