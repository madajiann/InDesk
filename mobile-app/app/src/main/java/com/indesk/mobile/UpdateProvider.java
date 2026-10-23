package com.indesk.mobile;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileNotFoundException;

/**
 * Minimal FileProvider replacement (no androidx dependency).
 * Serves only the downloaded update APK from app external files dir,
 * so the package installer can read it via content:// uri.
 */
public class UpdateProvider extends ContentProvider {

    public static final String AUTHORITY = "com.indesk.mobile.update";
    static final String FILE_NAME = "indesk-update.apk";

    private File updateFile() {
        return new File(getContext().getExternalFilesDir(null), FILE_NAME);
    }

    @Override
    public boolean onCreate() {
        return true;
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection,
                        String[] selectionArgs, String sortOrder) {
        File f = updateFile();
        MatrixCursor c = new MatrixCursor(new String[]{
                OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE});
        c.addRow(new Object[]{FILE_NAME, f.length()});
        return c;
    }

    @Override
    public String getType(Uri uri) {
        return "application/vnd.android.package-archive";
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        return null;
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        return 0;
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) {
        return 0;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        return ParcelFileDescriptor.open(updateFile(),
                ParcelFileDescriptor.MODE_READ_ONLY);
    }
}
