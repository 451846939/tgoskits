/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include <stddef.h>

void * memcpy( void * destination, const void * source, size_t length )
{
    unsigned char * out = destination;
    const unsigned char * in = source;
    while( length-- != 0U ) {
        *out++ = *in++;
    }
    return destination;
}

void * memset( void * destination, int value, size_t length )
{
    unsigned char * out = destination;
    while( length-- != 0U ) {
        *out++ = ( unsigned char ) value;
    }
    return destination;
}

void * memmove( void * destination, const void * source, size_t length )
{
    unsigned char * out = destination;
    const unsigned char * in = source;
    if( out < in ) {
        return memcpy( destination, source, length );
    }
    while( length-- != 0U ) {
        out[length] = in[length];
    }
    return destination;
}

int memcmp( const void * left, const void * right, size_t length )
{
    const unsigned char * a = left;
    const unsigned char * b = right;
    while( length-- != 0U ) {
        if( *a != *b ) {
            return ( int ) *a - ( int ) *b;
        }
        a++;
        b++;
    }
    return 0;
}

size_t strlen( const char * value )
{
    size_t length = 0;
    while( value[length] != '\0' ) {
        length++;
    }
    return length;
}
