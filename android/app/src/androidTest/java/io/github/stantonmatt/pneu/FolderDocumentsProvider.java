package io.github.stantonmatt.pneu;

import android.database.Cursor;
import android.database.MatrixCursor;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract.Document;
import android.provider.DocumentsProvider;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;

/** Disposable SAF provider. Remote fixture components cannot depend on the
 * target APK's Kotlin runtime, which is absent from this process's classpath. */
public class FolderDocumentsProvider extends DocumentsProvider {
    @Override public boolean onCreate() { return true; }
    private boolean pipeBacked=false;
    private File file() { return new File(getContext().getFilesDir(),"folder-fixture.txt"); }
    @Override public Bundle call(String method,String arg,Bundle extras) {
        if(method.equals("pipe-backed")) {pipeBacked=true;return new Bundle();}
        if(method.equals("fixture")) {
            pipeBacked=false;
            try(FileOutputStream out=new FileOutputStream(file())) { out.write((arg==null?"abc":arg).getBytes(StandardCharsets.UTF_8)); }
            catch(IOException e) { throw new IllegalStateException(e); }
            return new Bundle();
        }
        return super.call(method,arg,extras);
    }
    private Cursor rows(String[] ids,String[] projection) {
        String[] columns=projection==null?new String[]{Document.COLUMN_DOCUMENT_ID,Document.COLUMN_DISPLAY_NAME,Document.COLUMN_MIME_TYPE,Document.COLUMN_SIZE,Document.COLUMN_LAST_MODIFIED,Document.COLUMN_FLAGS}:projection;
        MatrixCursor cursor=new MatrixCursor(columns);
        for(String id:ids) {
            Object[] row=new Object[columns.length];
            for(int i=0;i<columns.length;i++) {
                switch(columns[i]) {
                    case Document.COLUMN_DOCUMENT_ID: row[i]=id;break;
                    case Document.COLUMN_DISPLAY_NAME:
                        switch(id) {case "root":row[i]="Trip";break;case "empty":row[i]="empty";break;case "link":row[i]="shortcut";break;case "pipe":row[i]="pipe";break;case "denied":row[i]="locked.txt";break;default:row[i]="a.txt";}
                        break;
                    case Document.COLUMN_MIME_TYPE:row[i]=(id.equals("root")||id.equals("empty"))?Document.MIME_TYPE_DIR:id.equals("link")?"inode/symlink":id.equals("pipe")?"inode/fifo":"text/plain";break;
                    case Document.COLUMN_SIZE:row[i]=id.equals("file")?file().length():0L;break;
                    case Document.COLUMN_LAST_MODIFIED:row[i]=0L;break;
                    case Document.COLUMN_FLAGS:row[i]=0;break;
                    default:row[i]=null;
                }
            }
            cursor.addRow(row);
        }
        return cursor;
    }
    @Override public Cursor queryRoots(String[] projection) { return new MatrixCursor(projection==null?new String[]{"root_id"}:projection); }
    @Override public Cursor queryDocument(String id,String[] projection) { return rows(new String[]{id},projection); }
    @Override public Cursor queryChildDocuments(String parent,String[] projection,String sortOrder) { return rows(parent.equals("root")?new String[]{"empty","file","link","pipe","denied"}:new String[0],projection); }
    @Override public boolean isChildDocument(String parent,String id) { return parent.equals("root") && (id.equals("root")||id.equals("empty")||id.equals("file")||id.equals("link")||id.equals("pipe")||id.equals("denied")); }
    @Override public ParcelFileDescriptor openDocument(String id,String mode,CancellationSignal signal) throws FileNotFoundException {
        if(!mode.equals("r")||id.equals("denied"))throw new FileNotFoundException("Unreadable fixture");
        if(id.equals("pipe")) {
            try {ParcelFileDescriptor[] pipe=ParcelFileDescriptor.createPipe();pipe[1].close();return pipe[0];}
            catch(IOException e) {FileNotFoundException failed=new FileNotFoundException("Pipe fixture");failed.initCause(e);throw failed;}
        }
        if(!id.equals("file"))throw new FileNotFoundException();
        if(pipeBacked) {
            try {
                ParcelFileDescriptor[] pipe=ParcelFileDescriptor.createPipe();
                byte[] bytes=java.nio.file.Files.readAllBytes(file().toPath());
                new Thread(()->{try(FileOutputStream out=new ParcelFileDescriptor.AutoCloseOutputStream(pipe[1])) {out.write(bytes);}catch(IOException ignored) {}}).start();
                return pipe[0];
            }catch(IOException e) {throw new FileNotFoundException(e.getMessage());}
        }
        return ParcelFileDescriptor.open(file(),ParcelFileDescriptor.MODE_READ_ONLY);
    }
}
