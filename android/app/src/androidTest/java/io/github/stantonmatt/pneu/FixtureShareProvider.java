package io.github.stantonmatt.pneu;

import android.content.*;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import java.io.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import android.os.Bundle;

public class FixtureShareProvider extends ContentProvider {
    private final List<ParcelFileDescriptor> pipes=new ArrayList<>();
    private volatile CountDownLatch entered=new CountDownLatch(1),release=new CountDownLatch(1);
    private final AtomicInteger opens=new AtomicInteger();
    private volatile String chooserState="";
    private Uri media;
    @Override public Bundle call(String method,String arg,Bundle extras) {
        if("reset".equals(method)){entered=new CountDownLatch(1);release=new CountDownLatch(1);opens.set(0);}
        if("release".equals(method))release.countDown();
        if("chooser-reset".equals(method))chooserState="";
        if("chooser-state".equals(method))chooserState=arg;
        if("delete-media".equals(method) && media!=null){getContext().getContentResolver().delete(media,null,null);media=null;}
        Bundle result=new Bundle();
        if("image-bytes".equals(method)) {
            try {result.putByteArray("bytes",java.nio.file.Files.readAllBytes(image().toPath()));}
            catch(IOException e){throw new IllegalStateException(e);}
        }
        if("create-media".equals(method)) {
            ContentValues values=new ContentValues();values.put("_display_name","Pneu_external_share.png");values.put("mime_type","image/png");values.put("relative_path","Pictures/Pneu tests/");values.put("is_pending",1);
            media=getContext().getContentResolver().insert(android.provider.MediaStore.Images.Media.getContentUri("external_primary"),values);
            android.graphics.Bitmap bitmap=android.graphics.Bitmap.createBitmap(16,16,android.graphics.Bitmap.Config.ARGB_8888);
            try(OutputStream output=getContext().getContentResolver().openOutputStream(media)){bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,output);}
            catch(IOException e){throw new IllegalStateException(e);}finally{bitmap.recycle();}
            values.clear();values.put("is_pending",0);getContext().getContentResolver().update(media,values,null,null);
            result.putString("uri",media.toString());
        }
        result.putBoolean("entered",entered.getCount()==0);result.putInt("opens",opens.get());result.putString("chooserState",chooserState);return result;
    }
    @Override public boolean onCreate(){return true;}
    private File image() throws IOException {
        File file=new File(getContext().getFilesDir(),"fixture.png");
        if(!file.exists()){
            android.graphics.Bitmap bitmap=android.graphics.Bitmap.createBitmap(48,48,android.graphics.Bitmap.Config.ARGB_8888);
            bitmap.eraseColor(android.graphics.Color.CYAN);
            try(FileOutputStream out=new FileOutputStream(file)){bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,out);}
            finally{bitmap.recycle();}
        }
        return file;
    }
    @Override public String getType(Uri uri){
        if("slow-image".equals(uri.getLastPathSegment())) {
            entered.countDown();try {if(!release.await(8,TimeUnit.SECONDS))throw new IllegalStateException("Fixture deadline");}catch(InterruptedException e){Thread.currentThread().interrupt();throw new IllegalStateException(e);}
        }
        return "image/png";
    }
    @Override public Cursor query(Uri uri,String[] projection,String selection,String[] args,String sort){
        MatrixCursor c=new MatrixCursor(new String[]{OpenableColumns.DISPLAY_NAME});c.addRow(new Object[]{"Caller fixture.png"});return c;
    }
    @Override public ParcelFileDescriptor openFile(Uri uri,String mode) throws FileNotFoundException {
        opens.incrementAndGet();
        if("denied".equals(uri.getLastPathSegment()))throw new SecurityException("Fixture denied");
        try {
            if("read-error".equals(uri.getLastPathSegment())) {
                ParcelFileDescriptor[] pipe=ParcelFileDescriptor.createReliablePipe();
                android.system.Os.write(pipe[1].getFileDescriptor(),new byte[]{1,2,3},0,3);
                pipe[1].closeWithError("Synthetic producer failure");return pipe[0];
            }
            if("stalled".equals(uri.getLastPathSegment())){
                ParcelFileDescriptor[] pipe=ParcelFileDescriptor.createPipe();pipes.add(pipe[1]);return pipe[0];
            }
            return ParcelFileDescriptor.open(image(),ParcelFileDescriptor.MODE_READ_ONLY);
        }catch(IOException|android.system.ErrnoException e){throw new FileNotFoundException(e.toString());}
    }
    @Override public Uri insert(Uri uri,ContentValues values){return null;}
    @Override public int delete(Uri uri,String selection,String[] args){return 0;}
    @Override public int update(Uri uri,ContentValues values,String selection,String[] args){return 0;}
}
