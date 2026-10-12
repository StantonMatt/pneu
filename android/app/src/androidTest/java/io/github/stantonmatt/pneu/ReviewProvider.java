package io.github.stantonmatt.pneu;

import android.content.*;
import android.content.res.AssetFileDescriptor;
import android.database.*;
import android.net.Uri;
import android.os.*;
import android.provider.OpenableColumns;
import java.io.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;

/** Test APK UID. All data is synthetic. Blocking points are explicitly released. */
public class ReviewProvider extends ContentProvider {
    private volatile CountDownLatch entered=new CountDownLatch(1), release=new CountDownLatch(1);
    private final AtomicInteger types=new AtomicInteger(), queries=new AtomicInteger(), opens=new AtomicInteger();
    @Override public boolean onCreate(){return true;}
    private void block(){entered.countDown();try {if(!release.await(20,TimeUnit.SECONDS))throw new IllegalStateException("Fixture deadline");}catch(InterruptedException e){Thread.currentThread().interrupt();throw new IllegalStateException(e);}}
    @Override public Bundle call(String method,String arg,Bundle extras){
        if("share-control".equals(method))return getContext().getContentResolver().call(Uri.parse("content://io.github.stantonmatt.pneu.test.sources"),arg,null,null);
        Bundle result=new Bundle();
        if("reset".equals(method)){entered=new CountDownLatch(1);release=new CountDownLatch(1);types.set(0);queries.set(0);opens.set(0);}
        if("entered".equals(method)){try{result.putBoolean("entered",entered.await(5,TimeUnit.SECONDS));}catch(InterruptedException e){Thread.currentThread().interrupt();}}
        if("release".equals(method))release.countDown();
        result.putInt("types",types.get());result.putInt("queries",queries.get());result.putInt("opens",opens.get());return result;
    }
    @Override public String getType(Uri uri){types.incrementAndGet();if("slow-type".equals(uri.getLastPathSegment()))block();return uri.getLastPathSegment().contains("image") || uri.getLastPathSegment().startsWith("slow") ? "image/png":"application/octet-stream";}
    @Override public Cursor query(Uri uri,String[] projection,String selection,String[] args,String sort){
        queries.incrementAndGet();if("slow-query".equals(uri.getLastPathSegment()))block();
        MatrixCursor cursor=new MatrixCursor(new String[]{OpenableColumns.DISPLAY_NAME});cursor.addRow(new Object[]{"Review fixture.png"});return cursor;
    }
    @Override public AssetFileDescriptor openAssetFile(Uri uri,String mode) throws FileNotFoundException {
        opens.incrementAndGet();String kind=uri.getLastPathSegment();if("slow-open".equals(kind))block();
        try {
            if("proxy-stall".equals(kind)) {
                HandlerThread worker=new HandlerThread("Pneu-fixture-proxy");worker.start();
                ParcelFileDescriptor proxy=getContext().getSystemService(android.os.storage.StorageManager.class).openProxyFileDescriptor(
                    ParcelFileDescriptor.MODE_READ_ONLY,new ProxyFileDescriptorCallback() {
                        @Override public long onGetSize(){return 4;}
                        @Override public int onRead(long offset,int size,byte[] data){block();int count=(int)Math.min(size,Math.max(0,4-offset));for(int n=0;n<count;n++)data[n]=(byte)(offset+n+1);return count;}
                        @Override public void onRelease(){worker.quitSafely();}
                    },new Handler(worker.getLooper()));
                return new AssetFileDescriptor(proxy,0,4);
            }
            if("short".equals(kind) || kind.startsWith("error") || "complete".equals(kind)) {
                ParcelFileDescriptor[] pipe=ParcelFileDescriptor.createReliablePipe();
                android.system.Os.write(pipe[1].getFileDescriptor(),new byte[]{1,2,3,4},0,4);
                if(kind.startsWith("error"))pipe[1].closeWithError("Synthetic producer failure");else pipe[1].close();
                return new AssetFileDescriptor(pipe[0],0,"short".equals(kind)?128: "error".equals(kind)?-1:4);
            }
            File file=new File(getContext().getFilesDir(),"review-image.png");
            if(!file.exists()) {
                android.graphics.Bitmap bitmap=android.graphics.Bitmap.createBitmap(24,24,android.graphics.Bitmap.Config.ARGB_8888);
                try(FileOutputStream output=new FileOutputStream(file)){bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,output);}bitmap.recycle();
            }
            return new AssetFileDescriptor(ParcelFileDescriptor.open(file,ParcelFileDescriptor.MODE_READ_ONLY),0,file.length());
        } catch(Exception e){throw new FileNotFoundException(e.toString());}
    }
    @Override public Uri insert(Uri uri,ContentValues values){return null;}
    @Override public int delete(Uri uri,String selection,String[] args){return 0;}
    @Override public int update(Uri uri,ContentValues values,String selection,String[] args){return 0;}
}
