package io.github.stantonmatt.pneu;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Binder;
import android.os.Bundle;
import android.provider.DocumentsContract;

/** Offers a persistable read of the fixture tree; the sender never receives
 * MANAGE_DOCUMENTS or broad storage access. Java keeps this remote component
 * independent of the target APK's Kotlin runtime. */
public class FolderGrantProvider extends ContentProvider {
    @Override public boolean onCreate() { return true; }
    @Override public Bundle call(String method,String arg,Bundle extras) {
        Context owner=getContext();
        Uri tree=DocumentsContract.buildTreeDocumentUri("io.github.stantonmatt.pneu.test.folders","root");
        long identity=Binder.clearCallingIdentity();
        try {
            if(method.equals("grant"))owner.grantUriPermission("io.github.stantonmatt.pneu",tree,Intent.FLAG_GRANT_READ_URI_PERMISSION|Intent.FLAG_GRANT_PREFIX_URI_PERMISSION|Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            else if(method.equals("revoke"))owner.revokeUriPermission(tree,Intent.FLAG_GRANT_READ_URI_PERMISSION);
            else throw new IllegalArgumentException("Unknown fixture command");
        }finally {Binder.restoreCallingIdentity(identity);}
        return new Bundle();
    }
    @Override public Cursor query(Uri uri,String[] projection,String selection,String[] args,String order) { return null; }
    @Override public String getType(Uri uri) { return null; }
    @Override public Uri insert(Uri uri,ContentValues values) { return null; }
    @Override public int delete(Uri uri,String selection,String[] args) { return 0; }
    @Override public int update(Uri uri,ContentValues values,String selection,String[] args) { return 0; }
}
